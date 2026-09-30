use base64::{Engine, engine::general_purpose::STANDARD};
use cshell_ipc::{
    ClipboardOperation, ClipboardResponse, ClipboardStatus, MAX_CLIPBOARD_TEXT_BYTES,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

const MAX_OSC_BYTES: usize = MAX_CLIPBOARD_TEXT_BYTES.div_ceil(3) * 4 + 6;
const LIFETIME: Duration = Duration::from_secs(30);
const COOLDOWN: Duration = Duration::from_secs(5);

struct Pending {
    token: [u8; 16],
    text: Zeroizing<String>,
    created: Instant,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.token.zeroize();
    }
}
#[derive(Default)]
struct State {
    pending: Option<Pending>,
    blocked: bool,
    last_capture: Option<Instant>,
}
#[derive(Clone, Default)]
pub(crate) struct ClipboardInbox(Arc<Mutex<State>>);
impl std::fmt::Debug for ClipboardInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardInbox").finish_non_exhaustive()
    }
}
impl ClipboardInbox {
    fn can_capture(&self, now: Instant) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        expire(&mut state, now);
        !state.blocked
            && state.pending.is_none()
            && state
                .last_capture
                .is_none_or(|last| now.duration_since(last) >= COOLDOWN)
    }
    pub(crate) fn expire_pending(&self) {
        expire(
            &mut self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Instant::now(),
        );
    }
    fn capture(&self, text: String, now: Instant) {
        let text = Zeroizing::new(text);
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        expire(&mut state, now);
        if state.blocked
            || state.pending.is_some()
            || state
                .last_capture
                .is_some_and(|last| now.duration_since(last) < COOLDOWN)
        {
            return;
        }
        state.pending = Some(Pending {
            token: rand::random(),
            text,
            created: now,
        });
        state.last_capture = Some(now);
    }

    pub(crate) fn handle(
        &self,
        operation: ClipboardOperation,
        token: &[u8],
        running: bool,
    ) -> ClipboardResponse {
        self.handle_at(operation, token, running, Instant::now())
    }

    fn handle_at(
        &self,
        operation: ClipboardOperation,
        token: &[u8],
        running: bool,
        now: Instant,
    ) -> ClipboardResponse {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        expire(&mut state, now);
        if !running {
            state.pending = None;
            state.blocked = true;
            return ClipboardResponse::with_status(ClipboardStatus::Unavailable);
        }
        if operation == ClipboardOperation::Poll {
            if !token.is_empty() {
                return ClipboardResponse::with_status(ClipboardStatus::InvalidRequest);
            }
            return match &state.pending {
                Some(pending) => ClipboardResponse {
                    token: pending.token.to_vec(),
                    byte_count: pending.text.len() as u32,
                    status: ClipboardStatus::Ok as i32,
                    text: String::new(),
                },
                None => ClipboardResponse::default(),
            };
        }
        if !state
            .pending
            .as_ref()
            .is_some_and(|pending| token == pending.token)
        {
            return ClipboardResponse::with_status(ClipboardStatus::Stale);
        }
        let Some(mut pending) = state.pending.take() else {
            return ClipboardResponse::with_status(ClipboardStatus::Stale);
        };
        if operation == ClipboardOperation::Block {
            state.blocked = true;
        }
        if operation == ClipboardOperation::Approve {
            ClipboardResponse {
                text: std::mem::take(&mut *pending.text),
                status: ClipboardStatus::Ok as i32,
                token: Vec::new(),
                byte_count: 0,
            }
        } else {
            ClipboardResponse::default()
        }
    }
}
fn expire(state: &mut State, now: Instant) {
    if state
        .pending
        .as_ref()
        .is_some_and(|pending| now.duration_since(pending.created) >= LIFETIME)
    {
        state.pending = None;
    }
}

/// Observes seven-bit OSC without changing the terminal's disabled OSC52 policy.
/// No payload is decoded until a complete sequence passes the hard size limit.
pub(crate) struct ClipboardObserver {
    escape: bool,
    osc: bool,
    overflow: bool,
    buffer: Zeroizing<Vec<u8>>,
}
impl Default for ClipboardObserver {
    fn default() -> Self {
        Self {
            escape: false,
            osc: false,
            overflow: false,
            buffer: Zeroizing::new(Vec::with_capacity(MAX_OSC_BYTES)),
        }
    }
}
impl ClipboardObserver {
    pub(crate) fn feed(&mut self, bytes: &[u8], inbox: &ClipboardInbox) {
        // Ordinary output needs no second byte-by-byte pass. Preserve scanner
        // state across chunks whenever an escape/string is already in progress.
        if !self.osc && !self.escape && !bytes.contains(&0x1b) {
            return;
        }
        for &byte in bytes {
            if byte == 0x18 || byte == 0x1a {
                self.reset();
                self.escape = false;
                continue;
            }
            if self.escape {
                self.escape = false;
                if self.osc && byte == b'\\' {
                    self.finish(inbox);
                    continue;
                }
                // An escape cancels the prior string unless it is ST.
                self.reset();
                if byte == b']' {
                    self.osc = true;
                }
                if byte == 0x1b {
                    self.escape = true;
                }
                continue;
            }
            if byte == 0x1b {
                self.escape = true;
                continue;
            }
            if !self.osc {
                continue;
            }
            if byte == 0x07 {
                self.finish(inbox);
                continue;
            }
            if self.buffer.len() == MAX_OSC_BYTES {
                self.overflow = true;
            }
            if !self.overflow {
                self.buffer.push(byte);
            }
        }
    }
    fn finish(&mut self, inbox: &ClipboardInbox) {
        if !self.overflow {
            let mut parts = self.buffer.split(|byte| *byte == b';');
            if let (Some(b"52"), Some(selection), Some(encoded), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
                && (selection.is_empty() || selection == b"c")
                && inbox.can_capture(Instant::now())
                && let Some(text) = decode_text(encoded)
            {
                inbox.capture(text, Instant::now());
            }
        }
        self.reset();
    }
    fn reset(&mut self) {
        self.buffer.as_mut_slice().zeroize();
        self.buffer.clear();
        self.osc = false;
        self.overflow = false;
    }
}

fn decode_text(encoded: &[u8]) -> Option<String> {
    // Reserve once so reallocations cannot leave previous clipboard bytes in
    // freed allocations. Even a partially decoded invalid payload is zeroized.
    let mut decoded = Zeroizing::new(Vec::with_capacity(encoded.len().div_ceil(4) * 3));
    STANDARD.decode_vec(encoded, &mut decoded).ok()?;
    if decoded.len() > MAX_CLIPBOARD_TEXT_BYTES {
        return None;
    }
    std::str::from_utf8(&decoded).ok().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enforces_decoded_size_limit_and_recovers_after_overflow() {
        for size in [MAX_CLIPBOARD_TEXT_BYTES, MAX_CLIPBOARD_TEXT_BYTES + 1] {
            let inbox = ClipboardInbox::default();
            let mut observer = ClipboardObserver::default();
            let encoded = STANDARD.encode(vec![b'a'; size]);
            observer.feed(format!("\x1b]52;;{encoded}\x07").as_bytes(), &inbox);
            let poll = inbox.handle(ClipboardOperation::Poll, &[], true);
            assert_eq!(!poll.token.is_empty(), size == MAX_CLIPBOARD_TEXT_BYTES);
            if size == MAX_CLIPBOARD_TEXT_BYTES {
                assert_eq!(poll.byte_count as usize, size);
            }
        }
        let inbox = ClipboardInbox::default();
        let mut observer = ClipboardObserver::default();
        observer.feed(
            format!(
                "\x1b]52;c;{}\x07\x1b]52;c;aGVsbG8=\x07",
                "A".repeat(MAX_OSC_BYTES * 2)
            )
            .as_bytes(),
            &inbox,
        );
        assert_eq!(
            inbox.handle(ClipboardOperation::Poll, &[], true).byte_count,
            5
        );
    }
    #[test]
    fn fragmented_write_requires_single_use_exact_token() {
        let inbox = ClipboardInbox::default();
        let mut observer = ClipboardObserver::default();
        for byte in b"\x1b]52;c;aGVsbG8=\x1b\\" {
            observer.feed(&[*byte], &inbox);
        }
        let poll = inbox.handle(ClipboardOperation::Poll, &[], true);
        assert_eq!(poll.byte_count, 5);
        assert!(poll.text.is_empty());
        assert_eq!(
            inbox
                .handle(ClipboardOperation::Approve, &[0; 16], true)
                .status,
            ClipboardStatus::Stale as i32
        );
        assert_eq!(
            inbox
                .handle(ClipboardOperation::Approve, &poll.token, true)
                .text,
            "hello"
        );
        assert_eq!(
            inbox
                .handle(ClipboardOperation::Approve, &poll.token, true)
                .status,
            ClipboardStatus::Stale as i32
        );
    }
    #[test]
    fn rejects_queries_selections_invalid_cancelled_and_oversized_sequences() {
        for bytes in [
            b"\x1b]52;c;?\x07".to_vec(),
            b"\x1b]52;p;aGVsbG8=\x07".to_vec(),
            b"\x1b]52;c;/w==\x07".to_vec(),
            b"\x1b]52;c;!!!!\x07".to_vec(),
            b"\x1b]52;c;aGVsbG8=\x18\x07".to_vec(),
            format!("\x1b]52;c;{}\x07", "A".repeat(MAX_OSC_BYTES * 2)).into_bytes(),
        ] {
            let inbox = ClipboardInbox::default();
            let mut observer = ClipboardObserver::default();
            observer.feed(&bytes, &inbox);
            assert!(
                inbox
                    .handle(ClipboardOperation::Poll, &[], true)
                    .token
                    .is_empty()
            );
            assert!(observer.buffer.capacity() <= MAX_OSC_BYTES.next_power_of_two());
        }
    }
    #[test]
    fn bounds_pending_cooldown_expiry_block_and_closed_session() {
        let inbox = ClipboardInbox::default();
        let now = Instant::now();
        inbox.capture("first".into(), now);
        inbox.capture("second".into(), now + COOLDOWN);
        let poll = inbox.handle_at(ClipboardOperation::Poll, &[], true, now);
        assert_eq!(poll.byte_count, 5);
        assert_eq!(
            inbox
                .handle_at(
                    ClipboardOperation::Approve,
                    &poll.token,
                    true,
                    now + LIFETIME
                )
                .status,
            ClipboardStatus::Stale as i32
        );
        inbox.capture("third".into(), now + LIFETIME);
        let poll = inbox.handle_at(ClipboardOperation::Poll, &[], true, now + LIFETIME);
        inbox.handle_at(ClipboardOperation::Block, &poll.token, true, now + LIFETIME);
        inbox.capture("fourth".into(), now + LIFETIME + COOLDOWN);
        assert!(
            inbox
                .handle_at(
                    ClipboardOperation::Poll,
                    &[],
                    true,
                    now + LIFETIME + COOLDOWN
                )
                .token
                .is_empty()
        );
        let inbox = ClipboardInbox::default();
        inbox.capture("closed".into(), now);
        assert_eq!(
            inbox
                .handle_at(ClipboardOperation::Poll, &[], false, now)
                .status,
            ClipboardStatus::Unavailable as i32
        );
        assert!(
            inbox
                .handle_at(ClipboardOperation::Poll, &[], true, now)
                .token
                .is_empty()
        );
        let inbox = ClipboardInbox::default();
        inbox.capture("cooldown".into(), now);
        let poll = inbox.handle_at(ClipboardOperation::Poll, &[], true, now);
        inbox.handle_at(ClipboardOperation::Reject, &poll.token, true, now);
        inbox.capture("too soon".into(), now + Duration::from_secs(1));
        assert!(
            inbox
                .handle_at(
                    ClipboardOperation::Poll,
                    &[],
                    true,
                    now + Duration::from_secs(1)
                )
                .token
                .is_empty()
        );
    }
}
