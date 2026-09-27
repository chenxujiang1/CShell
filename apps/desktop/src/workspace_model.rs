//! Runtime session bindings are deliberately absent from the saved layout.
use cshell_domain::*;
use std::collections::BTreeMap;

#[derive(Debug, Default)]
pub struct DesktopWorkspace {
    pub document: WorkspaceDocument,
    pub sessions: BTreeMap<TabId, SessionId>,
    pub pending_launch: Option<TabId>,
    pub writable: bool,
}

impl DesktopWorkspace {
    pub fn restore(document: WorkspaceDocument) -> Result<Self, String> {
        cshell_application::validate_workspace(&document).map_err(|e| e.to_string())?;
        if document.windows.len() != 1
            || document.tab_groups.len() != 1
            || document.panes.len() != 1
        {
            return Err("This workspace requires split/multiple-window support; the saved layout is preserved".into());
        }
        Ok(Self {
            document,
            writable: true,
            ..Default::default()
        })
    }
    pub fn active(&self) -> Option<TabId> {
        self.document.tab_groups[0].active_tab
    }
    pub fn select(&mut self, id: TabId) {
        if self.document.tab_groups[0].tabs.contains(&id) {
            self.document.tab_groups[0].active_tab = Some(id);
        }
    }
    pub fn add(&mut self, profile_id: Option<ProfileId>, title: String) -> Option<TabId> {
        if self.document.bindings.len() >= cshell_application::MAX_WORKSPACE_TABS {
            return None;
        }
        let id = TabId::new();
        self.document.bindings.push(WorkspaceBinding {
            tab_id: id,
            profile_id,
            title,
        });
        self.document.tab_groups[0].tabs.push(id);
        self.select(id);
        Some(id)
    }
    pub fn observe(
        &mut self,
        session: SessionId,
        profile: Option<ProfileId>,
        title: String,
    ) -> Option<TabId> {
        let id = if let Some(id) = self.pending_launch.take() {
            id
        } else if let Some((&id, _)) = self.sessions.iter().find(|(_, value)| **value == session) {
            id
        } else {
            self.add(profile, title.clone())?
        };
        self.sessions.insert(id, session);
        if let Some(binding) = self.document.bindings.iter_mut().find(|b| b.tab_id == id) {
            binding.profile_id = profile;
            binding.title = title;
        }
        self.select(id);
        Some(id)
    }
    pub fn remove(&mut self, id: TabId) {
        if self.pending_launch == Some(id) {
            self.pending_launch = None;
        }
        self.document.bindings.retain(|b| b.tab_id != id);
        let group = &mut self.document.tab_groups[0];
        let position = group.tabs.iter().position(|tab| *tab == id);
        group.tabs.retain(|tab| *tab != id);
        if group.active_tab == Some(id) {
            group.active_tab = position.and_then(|index| {
                group
                    .tabs
                    .get(index.min(group.tabs.len().saturating_sub(1)))
                    .copied()
            });
        }
        self.sessions.remove(&id);
    }
    pub fn move_tab(&mut self, id: TabId, delta: isize) {
        let tabs = &mut self.document.tab_groups[0].tabs;
        if let Some(index) = tabs.iter().position(|tab| *tab == id) {
            let target = index
                .saturating_add_signed(delta)
                .min(tabs.len().saturating_sub(1));
            tabs.swap(index, target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_layout_round_trips_but_desktop_refuses_to_flatten_it() {
        let mut doc = WorkspaceDocument::default();
        let first = doc.panes[0].id;
        let second = PaneId::new();
        let root = PaneId::new();
        let group = TabGroupId::new();
        doc.tab_groups.push(WorkspaceTabGroup {
            id: group,
            tabs: vec![],
            active_tab: None,
        });
        doc.panes.push(WorkspacePane {
            id: second,
            content: WorkspacePaneContent::Tabs { group_id: group },
        });
        doc.panes.push(WorkspacePane {
            id: root,
            content: WorkspacePaneContent::Split {
                axis: SplitAxis::Horizontal,
                ratio_permille: 500,
                first,
                second,
            },
        });
        doc.windows[0].root = root;
        let json = cshell_application::encode_workspace(&doc).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            cshell_application::decode_workspace(&json).unwrap_or_else(|e| panic!("{e}")),
            doc
        );
        assert!(DesktopWorkspace::restore(doc).is_err());
    }

    #[test]
    fn restore_retains_order_selection_and_profiles_without_live_sessions() {
        let mut workspace = DesktopWorkspace::default();
        let profile = ProfileId::new();
        let first = workspace
            .add(Some(profile), "First".into())
            .unwrap_or_else(|| panic!("tab"));
        let second = workspace
            .add(None, "Default".into())
            .unwrap_or_else(|| panic!("tab"));
        workspace.sessions.insert(first, SessionId::new());
        workspace.move_tab(second, -1);
        workspace.select(first);
        let restored =
            DesktopWorkspace::restore(workspace.document.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert!(restored.sessions.is_empty());
        assert!(restored.pending_launch.is_none());
        assert_eq!(restored.document.tab_groups[0].tabs, vec![second, first]);
        assert_eq!(restored.active(), Some(first));
        workspace.remove(first);
        assert_eq!(workspace.active(), Some(second));
        assert!(cshell_application::validate_workspace(&workspace.document).is_ok());
    }
}
