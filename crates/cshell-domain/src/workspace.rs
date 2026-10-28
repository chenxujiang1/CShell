//! Stable workspace layout. No UI toolkit objects or live process credentials.
use crate::{PaneId, ProfileId, TabGroupId, TabId, WorkspaceWindowId};
use serde::{Deserialize, Serialize};

pub const WORKSPACE_FORMAT: &str = "cshell.workspace";
pub const WORKSPACE_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDocument {
    pub format: String,
    pub version: u32,
    pub windows: Vec<WorkspaceWindow>,
    pub tab_groups: Vec<WorkspaceTabGroup>,
    pub panes: Vec<WorkspacePane>,
    pub bindings: Vec<WorkspaceBinding>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceWindow {
    pub id: WorkspaceWindowId,
    pub root: PaneId,
    pub focused_group: TabGroupId,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTabGroup {
    pub id: TabGroupId,
    pub tabs: Vec<TabId>,
    pub active_tab: Option<TabId>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePane {
    pub id: PaneId,
    pub content: WorkspacePaneContent,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspacePaneContent {
    Tabs {
        group_id: TabGroupId,
    },
    Split {
        axis: SplitAxis,
        ratio_permille: u16,
        first: PaneId,
        second: PaneId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

/// A saved Profile reference, never a command, credential, or live SessionId.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceBinding {
    pub tab_id: TabId,
    pub profile_id: Option<ProfileId>,
    pub title: String,
}

impl Default for WorkspaceDocument {
    fn default() -> Self {
        let group = TabGroupId::new();
        let root = PaneId::new();
        Self {
            format: WORKSPACE_FORMAT.into(),
            version: WORKSPACE_VERSION,
            windows: vec![WorkspaceWindow {
                id: WorkspaceWindowId::new(),
                root,
                focused_group: group,
            }],
            tab_groups: vec![WorkspaceTabGroup {
                id: group,
                tabs: vec![],
                active_tab: None,
            }],
            panes: vec![WorkspacePane {
                id: root,
                content: WorkspacePaneContent::Tabs { group_id: group },
            }],
            bindings: vec![],
        }
    }
}
