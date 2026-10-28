//! Explicit layout recovery and pane separation. Live sessions never enter files.
use crate::{MAX_WORKSPACE_BYTES, WorkspaceError, validate_workspace};
use cshell_domain::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
pub const MAX_WORKSPACE_REVIEW_BYTES: usize = MAX_WORKSPACE_BYTES * 2 + 32 * 1024;

pub fn workspace_window_groups(
    document: &WorkspaceDocument,
    window: WorkspaceWindowId,
) -> Vec<TabGroupId> {
    let Some(window) = document.windows.iter().find(|w| w.id == window) else {
        return Vec::new();
    };
    let mut pending = vec![window.root];
    let mut groups = Vec::new();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(pane) = document.panes.iter().find(|p| p.id == id) {
            match pane.content {
                WorkspacePaneContent::Tabs { group_id } => groups.push(group_id),
                WorkspacePaneContent::Split { first, second, .. } => {
                    pending.push(second);
                    pending.push(first);
                }
            }
        }
    }
    groups
}
pub fn validate_restorable_workspace(document: &WorkspaceDocument) -> Result<(), WorkspaceError> {
    validate_workspace(document)?;
    if document
        .windows
        .iter()
        .any(|w| w.id.as_uuid().is_nil() || workspace_window_groups(document, w.id).len() > 8)
        || document.panes.iter().any(|p| p.id.as_uuid().is_nil())
        || document.tab_groups.iter().any(|g| g.id.as_uuid().is_nil())
        || document.bindings.iter().any(|b| {
            b.tab_id.as_uuid().is_nil() || b.profile_id.is_some_and(|p| p.as_uuid().is_nil())
        })
    {
        return Err(WorkspaceError::InvalidLayout);
    }
    Ok(())
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRestorePlan {
    pub format: String,
    pub version: u32,
    pub storage_revision: u64,
    pub before: WorkspaceDocument,
    pub target: WorkspaceDocument,
    /// Eligible identities; the UI also requires an existing runtime binding.
    pub retain_tabs: BTreeSet<TabId>,
    pub detach_tabs: BTreeSet<TabId>,
    pub placeholder_tabs: BTreeSet<TabId>,
}
impl WorkspaceRestorePlan {
    pub fn new(
        storage_revision: u64,
        before: WorkspaceDocument,
        target: WorkspaceDocument,
    ) -> Result<Self, WorkspaceError> {
        validate_restorable_workspace(&before)?;
        validate_restorable_workspace(&target)?;
        let retain_tabs = target
            .bindings
            .iter()
            .filter(|target| {
                before
                    .bindings
                    .iter()
                    .any(|old| old.tab_id == target.tab_id && old.profile_id == target.profile_id)
            })
            .map(|b| b.tab_id)
            .collect::<BTreeSet<_>>();
        let detach_tabs = before
            .bindings
            .iter()
            .map(|b| b.tab_id)
            .filter(|id| !retain_tabs.contains(id))
            .collect();
        let placeholder_tabs = target
            .bindings
            .iter()
            .map(|b| b.tab_id)
            .filter(|id| !retain_tabs.contains(id))
            .collect();
        Ok(Self {
            format: "cshell.workspace-restore".into(),
            version: 1,
            storage_revision,
            before,
            target,
            retain_tabs,
            detach_tabs,
            placeholder_tabs,
        })
    }
    pub fn validate(&self) -> Result<(), WorkspaceError> {
        if *self
            != Self::new(
                self.storage_revision,
                self.before.clone(),
                self.target.clone(),
            )?
        {
            return Err(WorkspaceError::InvalidLayout);
        }
        if serde_json::to_vec(self)
            .map_err(|_| WorkspaceError::InvalidLayout)?
            .len()
            > MAX_WORKSPACE_REVIEW_BYTES
        {
            return Err(WorkspaceError::Limit);
        }
        Ok(())
    }
    pub fn fingerprint(&self) -> Result<[u8; 32], WorkspaceError> {
        self.validate()?;
        Ok(
            Sha256::digest(serde_json::to_vec(self).map_err(|_| WorkspaceError::InvalidLayout)?)
                .into(),
        )
    }
}
pub fn decode_workspace_restore(bytes: &[u8]) -> Result<WorkspaceRestorePlan, WorkspaceError> {
    if bytes.len() > MAX_WORKSPACE_REVIEW_BYTES {
        return Err(WorkspaceError::Limit);
    }
    let plan: WorkspaceRestorePlan =
        serde_json::from_slice(bytes).map_err(|_| WorkspaceError::InvalidLayout)?;
    plan.validate()?;
    Ok(plan)
}

/// Move a complete leaf group into a new window, retaining group/tab identities.
pub fn detach_workspace_group(
    document: &WorkspaceDocument,
    source: WorkspaceWindowId,
    group: TabGroupId,
) -> Result<(WorkspaceDocument, WorkspaceWindowId, Vec<TabId>), WorkspaceError> {
    validate_restorable_workspace(document)?;
    if document.windows.len() >= 8 {
        return Err(WorkspaceError::Limit);
    }
    if !workspace_window_groups(document, source).contains(&group) {
        return Err(WorkspaceError::InvalidLayout);
    }
    let tabs = document
        .tab_groups
        .iter()
        .find(|g| g.id == group)
        .ok_or(WorkspaceError::InvalidLayout)?
        .tabs
        .clone();
    if tabs.is_empty() {
        return Err(WorkspaceError::InvalidLayout);
    }
    let mut target = document.clone();
    let leaf = target
        .panes
        .iter()
        .find(|p| matches!(p.content,WorkspacePaneContent::Tabs{group_id}if group_id==group))
        .ok_or(WorkspaceError::InvalidLayout)?
        .id;
    let source_root = target
        .windows
        .iter()
        .find(|w| w.id == source)
        .ok_or(WorkspaceError::InvalidLayout)?
        .root;
    if source_root == leaf {
        let replacement = TabGroupId::new();
        target.tab_groups.push(WorkspaceTabGroup {
            id: replacement,
            tabs: Vec::new(),
            active_tab: None,
        });
        target
            .panes
            .iter_mut()
            .find(|p| p.id == leaf)
            .ok_or(WorkspaceError::InvalidLayout)?
            .content = WorkspacePaneContent::Tabs {
            group_id: replacement,
        };
        target
            .windows
            .iter_mut()
            .find(|w| w.id == source)
            .ok_or(WorkspaceError::InvalidLayout)?
            .focused_group = replacement;
    } else {
        let (parent, sibling) = target
            .panes
            .iter()
            .find_map(|p| match p.content {
                WorkspacePaneContent::Split { first, second, .. } if first == leaf => {
                    Some((p.id, second))
                }
                WorkspacePaneContent::Split { first, second, .. } if second == leaf => {
                    Some((p.id, first))
                }
                _ => None,
            })
            .ok_or(WorkspaceError::InvalidLayout)?;
        let content = target
            .panes
            .iter()
            .find(|p| p.id == sibling)
            .ok_or(WorkspaceError::InvalidLayout)?
            .content
            .clone();
        target
            .panes
            .iter_mut()
            .find(|p| p.id == parent)
            .ok_or(WorkspaceError::InvalidLayout)?
            .content = content;
        target.panes.retain(|p| p.id != leaf && p.id != sibling);
        let remaining = workspace_window_groups(&target, source);
        let window = target
            .windows
            .iter_mut()
            .find(|w| w.id == source)
            .ok_or(WorkspaceError::InvalidLayout)?;
        if window.focused_group == group {
            window.focused_group = *remaining.first().ok_or(WorkspaceError::InvalidLayout)?;
        }
    }
    let id = WorkspaceWindowId::new();
    let root = PaneId::new();
    target.windows.push(WorkspaceWindow {
        id,
        root,
        focused_group: group,
    });
    target.panes.push(WorkspacePane {
        id: root,
        content: WorkspacePaneContent::Tabs { group_id: group },
    });
    validate_restorable_workspace(&target)?;
    Ok((target, id, tabs))
}
