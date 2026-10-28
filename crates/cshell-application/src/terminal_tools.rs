use cshell_domain::*;
use serde::{Deserialize, Serialize};
pub const MAX_TERMINAL_TOOLS_BYTES: usize = 256 * 1024;
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalToolsCatalog {
    pub format: String,
    pub version: u32,
    pub revision: u64,
    pub highlights: Vec<HighlightSet>,
    pub triggers: Vec<TriggerRule>,
}
impl std::fmt::Debug for TerminalToolsCatalog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalToolsCatalog")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
impl Default for TerminalToolsCatalog {
    fn default() -> Self {
        Self {
            format: "cshell.terminal-tools".into(),
            version: 1,
            revision: 1,
            highlights: Vec::new(),
            triggers: Vec::new(),
        }
    }
}
fn name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn scope(profiles: &std::collections::BTreeSet<ProfileId>) -> bool {
    profiles.len() <= 128 && profiles.iter().all(|id| !id.as_uuid().is_nil())
}
impl TerminalToolsCatalog {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != "cshell.terminal-tools"
            || self.version != 1
            || self.revision == 0
            || self.highlights.len() > 16
            || self.triggers.len() > 64
            || self.highlights.iter().map(|s| s.rules.len()).sum::<usize>() > 64
        {
            return Err("terminal tools format/size limit");
        }
        let mut sets = std::collections::BTreeSet::new();
        for set in &self.highlights {
            if !name(&set.name) || !sets.insert(&set.name) || !scope(&set.profiles) {
                return Err("invalid highlight set/scope");
            }
            let mut ids = std::collections::BTreeSet::new();
            for rule in &set.rules {
                if !name(&rule.id) || !ids.insert(&rule.id) {
                    return Err("invalid highlight rule ID");
                }
                cshell_highlight::compile_pattern(&rule.pattern)?;
            }
        }
        let mut ids = std::collections::BTreeSet::new();
        for rule in &self.triggers {
            if !name(&rule.id)
                || !ids.insert(&rule.id)
                || !scope(&rule.profiles)
                || rule.cooldown_ms < 1000
                || rule.cooldown_ms > 3_600_000
                || !(1..=60).contains(&rule.per_minute)
            {
                return Err("invalid trigger rule/scope/rate");
            }
            cshell_highlight::compile_pattern(&rule.pattern)?;
            match &rule.action {
                TriggerAction::Notify { message } | TriggerAction::Record { label: message } => {
                    if message.is_empty()
                        || message.len() > 512
                        || message.chars().any(char::is_control)
                    {
                        return Err("invalid trigger event message");
                    }
                }
                TriggerAction::AppendText { text } => {
                    if text.is_empty() || text.len() > 1024 || text.contains('\0') {
                        return Err("invalid trigger text action");
                    }
                }
                TriggerAction::Script { script_id } => {
                    if !name(script_id) {
                        return Err("invalid trigger script reference");
                    }
                }
                TriggerAction::Highlight { .. } | TriggerAction::Bell => {}
            }
        }
        let mut display = self
            .highlights
            .iter()
            .flat_map(|set| set.rules.clone())
            .collect::<Vec<_>>();
        for rule in &self.triggers {
            if let TriggerAction::Highlight { style } = &rule.action {
                display.push(HighlightRule {
                    id: rule.id.clone(),
                    pattern: rule.pattern.clone(),
                    style: *style,
                    enabled: rule.enabled,
                });
            }
        }
        if display.len() > 64 {
            return Err("combined highlight rule limit");
        }
        cshell_highlight::CompiledHighlights::new(&display)?;
        Ok(())
    }
    pub fn highlight_rules(&self, profile: Option<ProfileId>) -> Vec<HighlightRule> {
        self.highlights
            .iter()
            .filter(|s| s.profiles.is_empty() || profile.is_some_and(|id| s.profiles.contains(&id)))
            .flat_map(|s| s.rules.clone())
            .collect()
    }
}
pub fn decode_terminal_tools(bytes: &[u8]) -> Result<TerminalToolsCatalog, &'static str> {
    if bytes.len() > MAX_TERMINAL_TOOLS_BYTES {
        return Err("terminal tools document limit");
    }
    let value: TerminalToolsCatalog =
        serde_json::from_slice(bytes).map_err(|_| "invalid terminal tools JSON")?;
    value.validate()?;
    Ok(value)
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighlightDocument {
    pub format: String,
    pub version: u32,
    pub sets: Vec<HighlightSet>,
}
impl HighlightDocument {
    pub fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() > MAX_TERMINAL_TOOLS_BYTES {
            return Err("highlight document limit");
        }
        let value: Self =
            serde_json::from_slice(bytes).map_err(|_| "invalid highlight document")?;
        if value.format != "cshell.highlight-sets" || value.version != 1 {
            return Err("unsupported highlight document");
        }
        TerminalToolsCatalog {
            highlights: value.sets.clone(),
            ..Default::default()
        }
        .validate()?;
        Ok(value)
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawJournalSlice {
    pub session: SessionId,
    pub sequence: u64,
    pub offset: u64,
    pub record_bytes: u64,
    pub total_records: u64,
    pub data: Vec<u8>,
    pub elapsed_us: Option<u64>,
    pub rows: Option<u16>,
    pub cols: Option<u16>,
}
impl RawJournalSlice {
    pub fn confirm(
        &self,
        session: SessionId,
        sequence: u64,
        offset: u64,
    ) -> Result<(), &'static str> {
        if self.session != session
            || self.sequence != sequence
            || self.offset != offset
            || self.data.len() > 64 * 1024
            || self.record_bytes > 16 * 1024 * 1024
            || offset > self.record_bytes
            || offset
                .checked_add(self.data.len() as u64)
                .is_none_or(|end| end > self.record_bytes)
            || (sequence >= self.total_records
                && (self.record_bytes != 0 || offset != 0 || !self.data.is_empty()))
            || (sequence < self.total_records && offset < self.record_bytes && self.data.is_empty())
            || self.rows.is_some() != self.cols.is_some()
            || self.rows.is_some_and(|n| n == 0)
            || self.cols.is_some_and(|n| n == 0)
        {
            return Err("raw journal scope/range mismatch");
        }
        Ok(())
    }
    pub fn next_cursor(&self) -> (u64, u64) {
        let end = self.offset + self.data.len() as u64;
        if self.sequence < self.total_records && end == self.record_bytes {
            (self.sequence.saturating_add(1), 0)
        } else {
            (self.sequence, end)
        }
    }
}
impl std::fmt::Debug for RawJournalSlice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RawJournalSlice")
            .field("sequence", &self.sequence)
            .field("bytes", &self.data.len())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerEvent {
    pub id: u64,
    pub session: SessionId,
    pub run: TaskRunId,
    pub rule_id: String,
    pub output_sequence: u64,
    pub action: TriggerAction,
    pub disposition: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerRuntimeView {
    pub session: SessionId,
    pub run: TaskRunId,
    pub catalog_revision: u64,
    pub allow_text: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerminalToolsView {
    pub catalog: TerminalToolsCatalog,
    pub armed: Vec<TriggerRuntimeView>,
    pub events: Vec<TriggerEvent>,
    pub next_event: u64,
    pub lost_before: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TerminalToolsReply {
    View(TerminalToolsView),
    Raw(RawJournalSlice),
    Shell(ShellHistory),
}
pub enum TerminalToolsExpectation<'a> {
    Shell {session: SessionId, id:u64},
    View {
        after: u64,
    },
    Saved(&'a TerminalToolsCatalog),
    Armed {
        session: SessionId,
        revision: u64,
        allow_text: bool,
    },
    Disarmed(SessionId),
    Raw {
        session: SessionId,
        sequence: u64,
        offset: u64,
    },
}
pub fn decode_terminal_tools_reply(
    bytes: &[u8],
    expect: TerminalToolsExpectation<'_>,
) -> Result<TerminalToolsReply, &'static str> {
    if bytes.len() > 512 * 1024 {
        return Err("terminal tools reply limit");
    }
    let reply: TerminalToolsReply =
        serde_json::from_slice(bytes).map_err(|_| "invalid terminal tools reply")?;
    if let TerminalToolsReply::View(view) = &reply {
        view.catalog.validate()?;
        if view.armed.len() > 16 || view.events.len() > 128 || view.lost_before > view.next_event {
            return Err("terminal tools runtime limit");
        }
        let mut sessions = std::collections::BTreeSet::new();
        for arm in &view.armed {
            if arm.session.as_uuid().is_nil()
                || arm.run.as_uuid().is_nil()
                || !sessions.insert(arm.session)
                || arm.catalog_revision != view.catalog.revision
            {
                return Err("invalid trigger runtime binding");
            }
        }
        let mut previous = 0;
        for event in &view.events {
            let action_valid = match &event.action {
                TriggerAction::Notify { message } | TriggerAction::Record { label: message } => {
                    !message.is_empty()
                        && message.len() <= 512
                        && !message.chars().any(char::is_control)
                }
                TriggerAction::AppendText { text } => {
                    !text.is_empty() && text.len() <= 1024 && !text.contains('\0')
                }
                TriggerAction::Script { script_id } => name(script_id),
                TriggerAction::Highlight { .. } | TriggerAction::Bell => true,
            };
            if event.id <= previous
                || event.id > view.next_event
                || event.session.as_uuid().is_nil()
                || event.run.as_uuid().is_nil()
                || !name(&event.rule_id)
                || event.disposition.len() > 128
                || !action_valid
                || !matches!(
                    event.disposition.as_str(),
                    "recorded"
                        | "accepted-once"
                        | "dispatch-uncertain"
                        | "text-permission-required"
                        | "script-capability-required"
                        | "blocked"
                )
            {
                return Err("invalid trigger event");
            }
            previous = event.id;
        }
    }
    match (&reply, expect) {
        (TerminalToolsReply::Shell(value), TerminalToolsExpectation::Shell {session,id}) => {
            if !value.validate(session,id) {return Err("shell history scope or limit mismatch");}
        }
        (
            TerminalToolsReply::Raw(raw),
            TerminalToolsExpectation::Raw {
                session,
                sequence,
                offset,
            },
        ) => raw.confirm(session, sequence, offset)?,
        (TerminalToolsReply::View(view), TerminalToolsExpectation::View { after }) => {
            if view.events.iter().any(|e| e.id <= after) || view.next_event < after {
                return Err("trigger event cursor mismatch");
            }
        }
        (TerminalToolsReply::View(view), TerminalToolsExpectation::Saved(proposed)) => {
            let mut expected = proposed.clone();
            expected.revision = expected
                .revision
                .checked_add(1)
                .ok_or("terminal tools revision exhausted")?;
            if view.catalog != expected || !view.armed.is_empty() {
                return Err("terminal tools save effect mismatch");
            }
        }
        (
            TerminalToolsReply::View(view),
            TerminalToolsExpectation::Armed {
                session,
                revision,
                allow_text,
            },
        ) => {
            if view.catalog.revision != revision
                || !view.armed.iter().any(|a| {
                    a.session == session
                        && a.catalog_revision == revision
                        && a.allow_text == allow_text
                })
            {
                return Err("trigger arm scope mismatch");
            }
        }
        (TerminalToolsReply::View(view), TerminalToolsExpectation::Disarmed(session)) => {
            if view.armed.iter().any(|a| a.session == session) {
                return Err("trigger disarm scope mismatch");
            }
        }
        _ => return Err("terminal tools reply type mismatch"),
    }
    Ok(reply)
}
