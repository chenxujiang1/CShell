//! Saved command templates contain configuration, never live session bindings.
use crate::{HostSelection, QuickCommandFolderId, QuickCommandId, TaskDefinition};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickCommandTarget {
    #[default]
    Current,
    Armed,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickCommandSystem {
    Windows,
    Linux,
    Macos,
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum QuickCommandAction {
    SendText {
        text: String,
        append_enter: bool,
        target: QuickCommandTarget,
    },
    OpenCompose {
        text: String,
        append_enter: bool,
        multiline: bool,
        target: QuickCommandTarget,
    },
    /// Open a task draft; host resolution, saving and starting remain explicit.
    RunTask {
        definition: TaskDefinition,
        selection: HostSelection,
    },
    /// A task template restricted to one static script step.
    RunScript {
        definition: TaskDefinition,
        selection: HostSelection,
    },
}
impl std::fmt::Debug for QuickCommandAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuickCommandAction")
            .field("text", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuickCommandFolder {
    pub id: QuickCommandFolderId,
    pub name: String,
    pub parent_id: Option<QuickCommandFolderId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuickCommandRecord {
    pub id: QuickCommandId,
    pub name: String,
    pub folder_id: Option<QuickCommandFolderId>,
    pub tags: BTreeSet<String>,
    pub systems: BTreeSet<QuickCommandSystem>,
    pub action: QuickCommandAction,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuickCommandCatalog {
    pub format: String,
    pub version: u32,
    pub folders: Vec<QuickCommandFolder>,
    pub commands: Vec<QuickCommandRecord>,
}
impl Default for QuickCommandCatalog {
    fn default() -> Self {
        Self {
            format: "cshell.quick-commands".into(),
            version: 1,
            folders: Vec::new(),
            commands: Vec::new(),
        }
    }
}
