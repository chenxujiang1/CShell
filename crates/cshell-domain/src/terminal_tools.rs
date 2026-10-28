use crate::ProfileId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OutputPattern {
    Literal { text: String, case_sensitive: bool },
    Regex { text: String, case_sensitive: bool },
}
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighlightStyle {
    pub foreground: Option<[u8; 3]>,
    pub background: Option<[u8; 3]>,
    pub underline: bool,
}
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighlightRule {
    pub id: String,
    pub pattern: OutputPattern,
    pub style: HighlightStyle,
    pub enabled: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighlightSet {
    pub name: String,
    pub profiles: BTreeSet<ProfileId>,
    pub rules: Vec<HighlightRule>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerAction {
    Highlight { style: HighlightStyle },
    Notify { message: String },
    Bell,
    AppendText { text: String },
    Record { label: String },
    Script { script_id: String },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerRule {
    pub id: String,
    pub profiles: BTreeSet<ProfileId>,
    pub pattern: OutputPattern,
    pub action: TriggerAction,
    pub cooldown_ms: u32,
    pub per_minute: u16,
    pub enabled: bool,
}
