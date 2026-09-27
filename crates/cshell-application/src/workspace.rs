//! Workspace persistence port and bounded, toolkit-independent layout validation.
use async_trait::async_trait;
use cshell_domain::*;
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const MAX_WORKSPACE_BYTES: usize = 256 * 1024;
pub const MAX_WORKSPACE_TABS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSnapshot {
    pub revision: u64,
    pub document: WorkspaceDocument,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WorkspaceError {
    #[error("unsupported workspace format or version")]
    UnsupportedVersion,
    #[error("workspace exceeds its size or topology limits")]
    Limit,
    #[error("workspace contains duplicate IDs, missing bindings or invalid layout references")]
    InvalidLayout,
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum WorkspaceRepositoryError {
    #[error("workspace revision changed: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("workspace storage is unavailable")]
    Unavailable,
    #[error("stored workspace is corrupt")]
    Corrupt,
}

#[async_trait]
pub trait WorkspaceRepository: std::fmt::Debug + Send + Sync {
    async fn load_workspace(&self) -> Result<Option<WorkspaceSnapshot>, WorkspaceRepositoryError>;
    async fn save_workspace(
        &self,
        expected_revision: u64,
        document: &WorkspaceDocument,
    ) -> Result<WorkspaceSnapshot, WorkspaceRepositoryError>;
}

pub fn encode_workspace(document: &WorkspaceDocument) -> Result<Vec<u8>, WorkspaceError> {
    validate_workspace(document)?;
    serde_json::to_vec(document).map_err(|_| WorkspaceError::InvalidLayout)
}

pub fn decode_workspace(bytes: &[u8]) -> Result<WorkspaceDocument, WorkspaceError> {
    if bytes.len() > MAX_WORKSPACE_BYTES {
        return Err(WorkspaceError::Limit);
    }
    let document = serde_json::from_slice(bytes).map_err(|_| WorkspaceError::InvalidLayout)?;
    validate_workspace(&document)?;
    Ok(document)
}

pub fn validate_workspace(document: &WorkspaceDocument) -> Result<(), WorkspaceError> {
    if document.format != WORKSPACE_FORMAT || document.version != WORKSPACE_VERSION {
        return Err(WorkspaceError::UnsupportedVersion);
    }
    if document.windows.is_empty()
        || document.windows.len() > 8
        || document.panes.len() > 128
        || document.tab_groups.len() > 64
        || document.bindings.len() > MAX_WORKSPACE_TABS
        || document.bindings.iter().any(|b| {
            b.title.is_empty() || b.title.len() > 256 || b.title.chars().any(char::is_control)
        })
        || serde_json::to_vec(document)
            .map_err(|_| WorkspaceError::InvalidLayout)?
            .len()
            > MAX_WORKSPACE_BYTES
    {
        return Err(WorkspaceError::Limit);
    }
    let panes: BTreeMap<_, _> = document.panes.iter().map(|p| (p.id, p)).collect();
    let groups: BTreeMap<_, _> = document.tab_groups.iter().map(|g| (g.id, g)).collect();
    let bindings: BTreeSet<_> = document.bindings.iter().map(|b| b.tab_id).collect();
    if panes.len() != document.panes.len()
        || groups.len() != document.tab_groups.len()
        || bindings.len() != document.bindings.len()
        || document
            .windows
            .iter()
            .map(|w| w.id)
            .collect::<BTreeSet<_>>()
            .len()
            != document.windows.len()
    {
        return Err(WorkspaceError::InvalidLayout);
    }
    let mut seen_panes = BTreeSet::new();
    let mut seen_groups = BTreeSet::new();
    let mut seen_tabs = BTreeSet::new();
    for window in &document.windows {
        let mut window_groups = BTreeSet::new();
        let mut pending = vec![(window.root, 0)];
        while let Some((id, depth)) = pending.pop() {
            if depth > 16 {
                return Err(WorkspaceError::Limit);
            }
            if !seen_panes.insert(id) {
                return Err(WorkspaceError::InvalidLayout);
            }
            let pane = panes.get(&id).ok_or(WorkspaceError::InvalidLayout)?;
            match pane.content {
                WorkspacePaneContent::Tabs { group_id } => {
                    if !seen_groups.insert(group_id) {
                        return Err(WorkspaceError::InvalidLayout);
                    }
                    window_groups.insert(group_id);
                    let group = groups.get(&group_id).ok_or(WorkspaceError::InvalidLayout)?;
                    if group.tabs.len() > MAX_WORKSPACE_TABS
                        || group.active_tab.is_some_and(|id| !group.tabs.contains(&id))
                        || (group.tabs.is_empty() != group.active_tab.is_none())
                    {
                        return Err(WorkspaceError::InvalidLayout);
                    }
                    for tab in &group.tabs {
                        if !seen_tabs.insert(*tab) || !bindings.contains(tab) {
                            return Err(WorkspaceError::InvalidLayout);
                        }
                    }
                }
                WorkspacePaneContent::Split {
                    ratio_permille,
                    first,
                    second,
                    ..
                } => {
                    if !(100..=900).contains(&ratio_permille) {
                        return Err(WorkspaceError::InvalidLayout);
                    }
                    pending.push((second, depth + 1));
                    pending.push((first, depth + 1));
                }
            }
        }
        if !window_groups.contains(&window.focused_group) {
            return Err(WorkspaceError::InvalidLayout);
        }
    }
    if seen_panes.len() != panes.len() || seen_groups.len() != groups.len() || seen_tabs != bindings
    {
        return Err(WorkspaceError::InvalidLayout);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn topology_rejects_cycles_orphans_duplicates_and_invalid_focus() {
        let good = WorkspaceDocument::default();
        assert_eq!(validate_workspace(&good), Ok(()));
        let mut broken = good.clone();
        broken.panes[0].content = WorkspacePaneContent::Split {
            axis: SplitAxis::Horizontal,
            ratio_permille: 500,
            first: broken.panes[0].id,
            second: broken.panes[0].id,
        };
        assert_eq!(
            validate_workspace(&broken),
            Err(WorkspaceError::InvalidLayout)
        );
        let mut broken = good.clone();
        broken.panes.push(WorkspacePane {
            id: PaneId::new(),
            content: good.panes[0].content.clone(),
        });
        assert_eq!(
            validate_workspace(&broken),
            Err(WorkspaceError::InvalidLayout)
        );
        let mut broken = good.clone();
        broken.windows[0].focused_group = TabGroupId::new();
        assert_eq!(
            validate_workspace(&broken),
            Err(WorkspaceError::InvalidLayout)
        );
        let mut broken = good;
        broken.version += 1;
        assert_eq!(
            validate_workspace(&broken),
            Err(WorkspaceError::UnsupportedVersion)
        );
        assert!(
            decode_workspace(
                br#"{"format":"cshell.workspace","version":1,"password":"forbidden"}"#
            )
            .is_err()
        );
    }
}
