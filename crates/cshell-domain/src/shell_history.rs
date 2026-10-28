//! Ephemeral shell-reported boundaries; never an authorization or process identity.
use crate::SessionId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellCommand {
    pub id: u64,
    pub line: u64,
    pub output_line: u64,
    pub output_column: u16,
    pub end_line: Option<u64>,
    pub end_column: u16,
    pub command: String,
    pub command_complete: bool,
    pub exit_status: Option<i32>,
    pub elapsed_ms: Option<u64>,
    pub running: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellHistory {
    pub session: SessionId,
    pub supported: bool,
    pub first_line: u64,
    pub cwd: Option<String>,
    pub commands: Vec<ShellCommand>,
    pub output_id: u64,
    pub output: Option<String>,
    pub truncated: bool,
}

impl ShellHistory {
    pub fn validate(&self, session: SessionId, id: u64) -> bool {
        self.session == session
            && self.output_id == id
            && self.commands.len() <= 128
            && self.commands.iter().map(|c| c.command.len()).sum::<usize>() <= 128 * 1024
            && self.commands.windows(2).all(|w| w[0].id < w[1].id)
            && self
                .commands
                .iter()
                .all(|c| c.id != 0 && c.command.len() <= 4096)
            && self
                .cwd
                .as_ref()
                .is_none_or(|s| s.len() <= 4096 && !s.chars().any(char::is_control))
            && self.output.as_ref().is_none_or(|s| s.len() <= 65536)
            && (id == 0 || self.commands.iter().any(|c| c.id == id))
    }
}
