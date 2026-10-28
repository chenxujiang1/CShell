//! Live-output trigger matcher. Replay/render callers never own this engine.
use cshell_domain::{ProfileId, TriggerAction, TriggerRule};
use regex::Regex;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};
#[derive(Clone, Debug)]
pub struct TriggerMatch {
    pub rule_id: String,
    pub sequence: u64,
    pub action: TriggerAction,
}
#[derive(Debug)]
struct Rule {
    definition: TriggerRule,
    regex: Regex,
    last: Option<Instant>,
    recent: VecDeque<Instant>,
    text_used: bool,
    last_sequence: Option<u64>,
}
#[derive(Debug)]
pub struct TriggerEngine {
    rules: Vec<Rule>,
    window: String,
    offset: u64,
    last_sequence: Option<u64>,
    last_offset: u64,
    decoder: OutputDecoder,
}
impl TriggerEngine {
    pub fn new(rules: &[TriggerRule], profile: Option<ProfileId>) -> Result<Self, &'static str> {
        if rules.len() > 64 {
            return Err("trigger rule limit");
        }
        let mut compiled = Vec::new();
        for rule in rules.iter().filter(|r| {
            r.enabled
                && (r.profiles.is_empty() || profile.is_some_and(|id| r.profiles.contains(&id)))
        }) {
            if rule.cooldown_ms < 1000
                || rule.cooldown_ms > 3_600_000
                || !(1..=60).contains(&rule.per_minute)
            {
                return Err("trigger cooldown/rate limit");
            }
            compiled.push(Rule {
                definition: rule.clone(),
                regex: cshell_highlight::compile_pattern(&rule.pattern)?,
                last: None,
                recent: VecDeque::new(),
                text_used: false,
                last_sequence: None,
            });
        }
        Ok(Self {
            rules: compiled,
            window: String::new(),
            offset: 0,
            last_sequence: None,
            last_offset: 0,
            decoder: OutputDecoder::default(),
        })
    }
    pub fn feed(
        &mut self,
        sequence: u64,
        record_offset: u64,
        bytes: &[u8],
        now: Instant,
    ) -> Vec<TriggerMatch> {
        if self
            .last_sequence
            .is_some_and(|s| sequence < s || (sequence == s && record_offset < self.last_offset))
        {
            return Vec::new();
        }
        if self.last_sequence.is_some_and(|s| {
            (sequence == s && record_offset != self.last_offset)
                || (sequence > s && (sequence != s.saturating_add(1) || record_offset != 0))
        }) {
            self.window.clear();
            self.decoder = OutputDecoder::default();
            self.offset = 0;
        }
        self.last_sequence = Some(sequence);
        self.last_offset = record_offset.saturating_add(bytes.len() as u64);
        let mut out = Vec::new();
        for chunk in bytes.chunks(1024) {
            let decoded = self.decoder.feed(chunk);
            let old_end = self.offset + self.window.len() as u64;
            self.window.push_str(&decoded);
            // Maximum cross-packet context is explicit and independent of an
            // adversarial never-ending line. Matches ending in old context skip.
            if self.window.len() > 8192 {
                let mut cut = self.window.len() - 8192;
                while !self.window.is_char_boundary(cut) {
                    cut += 1;
                }
                self.window.drain(..cut);
                self.offset += cut as u64;
            }
            for rule in &mut self.rules {
                while rule
                    .recent
                    .front()
                    .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
                {
                    rule.recent.pop_front();
                }
                if rule.last_sequence == Some(sequence)
                    || rule.last.is_some_and(|t| {
                        now.duration_since(t)
                            < Duration::from_millis(u64::from(rule.definition.cooldown_ms))
                    })
                    || rule.recent.len() >= usize::from(rule.definition.per_minute)
                    || rule.text_used
                {
                    continue;
                }
                if rule
                    .regex
                    .find_iter(&self.window)
                    .any(|m| m.start() != m.end() && self.offset + m.end() as u64 > old_end)
                {
                    rule.last = Some(now);
                    rule.recent.push_back(now);
                    rule.last_sequence = Some(sequence);
                    if matches!(rule.definition.action, TriggerAction::AppendText { .. }) {
                        rule.text_used = true;
                    }
                    out.push(TriggerMatch {
                        rule_id: rule.definition.id.clone(),
                        sequence,
                        action: rule.definition.action.clone(),
                    });
                }
            }
        }
        out
    }
}
/// Stateful ANSI/control stripping and split UTF-8 assembly. Never emits OSC,
/// clipboard, DCS or terminal responses to a command dispatcher.
#[derive(Debug, Default)]
pub struct OutputDecoder {
    state: u8,
    pending: Vec<u8>,
}
impl OutputDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> String {
        let mut visible = Vec::new();
        for &b in bytes {
            match self.state {
                0 => match b {
                    0x1b => self.state = 1,
                    b'\r' => {}
                    b'\n' | b'\t' => visible.push(b),
                    0..=31 | 127 => {}
                    _ => visible.push(b),
                },
                1 => {
                    self.state = match b {
                        b'[' => 2,
                        b']' => 3,
                        b'P' | b'X' | b'^' | b'_' => 4,
                        _ => 0,
                    }
                }
                2 => {
                    if (0x40..=0x7e).contains(&b) {
                        self.state = 0;
                    }
                }
                3 => match b {
                    7 => self.state = 0,
                    0x1b => self.state = 5,
                    _ => {}
                },
                4 => {
                    if b == 0x1b {
                        self.state = 6;
                    }
                }
                5 | 6 => {
                    let previous = self.state;
                    self.state = if b == b'\\' {
                        0
                    } else if b == 0x1b {
                        previous
                    } else if previous == 5 {
                        3
                    } else {
                        4
                    };
                }
                _ => self.state = 0,
            }
        }
        self.pending.extend(visible);
        let mut result = String::new();
        let mut consumed = 0;
        loop {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(s) => {
                    result.push_str(s);
                    consumed = self.pending.len();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        if let Ok(s) =
                            std::str::from_utf8(&self.pending[consumed..consumed + valid])
                        {
                            result.push_str(s);
                        }
                        consumed += valid;
                    }
                    match e.error_len() {
                        Some(n) => {
                            result.push('\u{fffd}');
                            consumed += n;
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        result
    }
}
