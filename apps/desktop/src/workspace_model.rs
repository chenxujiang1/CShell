//! Runtime session bindings are deliberately absent from the saved layout.
use cshell_domain::*;
use std::collections::BTreeMap;

#[derive(Debug)]
pub struct DesktopWorkspace {
    pub document: WorkspaceDocument,
    pub sessions: BTreeMap<TabId, SessionId>,
    pub pending_launch: Option<TabId>,
    pub writable: bool,
    pub window_id: WorkspaceWindowId,
}

impl Default for DesktopWorkspace {
    fn default() -> Self {
        let document = WorkspaceDocument::default();
        let window_id = document.windows[0].id;
        Self {
            document,
            sessions: BTreeMap::new(),
            pending_launch: None,
            writable: false,
            window_id,
        }
    }
}

impl DesktopWorkspace {
    pub fn restore(document: WorkspaceDocument) -> Result<Self, String> {
        cshell_application::validate_workspace(&document).map_err(|e| e.to_string())?;
        if document
            .windows
            .iter()
            .any(|w| Self::groups_in(&document, w.id).len() > 8)
        {
            return Err("This workspace exceeds the eight-pane-per-window limit; the saved layout is preserved".into());
        }
        let window_id = document.windows[0].id;
        Ok(Self {
            document,
            window_id,
            writable: true,
            ..Default::default()
        })
    }
    fn groups_in(document: &WorkspaceDocument, window: WorkspaceWindowId) -> Vec<TabGroupId> {
        let Some(root) = document
            .windows
            .iter()
            .find(|w| w.id == window)
            .map(|w| w.root)
        else {
            return Vec::new();
        };
        let mut groups = Vec::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
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
    pub fn window_of_tab(&self, tab: TabId) -> Option<WorkspaceWindowId> {
        let group = self
            .document
            .tab_groups
            .iter()
            .find(|g| g.tabs.contains(&tab))?
            .id;
        self.document
            .windows
            .iter()
            .find(|w| Self::groups_in(&self.document, w.id).contains(&group))
            .map(|w| w.id)
    }
    pub fn active(&self) -> Option<TabId> {
        self.document
            .tab_groups
            .iter()
            .find(|g| g.id == self.focused_group())
            .and_then(|g| g.active_tab)
    }
    pub fn focused_group(&self) -> TabGroupId {
        self.document
            .windows
            .iter()
            .find(|w| w.id == self.window_id)
            .map_or(self.document.windows[0].focused_group, |w| w.focused_group)
    }
    pub fn visible_tabs(&self) -> Vec<TabId> {
        let groups = Self::groups_in(&self.document, self.window_id);
        self.document
            .tab_groups
            .iter()
            .filter(|g| groups.contains(&g.id))
            .filter_map(|g| g.active_tab)
            .collect()
    }
    pub fn select(&mut self, id: TabId) {
        if self.window_of_tab(id) != Some(self.window_id) {
            return;
        }
        if let Some(group) = self
            .document
            .tab_groups
            .iter_mut()
            .find(|g| g.tabs.contains(&id))
        {
            group.active_tab = Some(id);
            if let Some(window) = self
                .document
                .windows
                .iter_mut()
                .find(|w| w.id == self.window_id)
            {
                window.focused_group = group.id;
            }
        }
    }
    pub fn new_window(&mut self) -> Option<WorkspaceWindowId> {
        if self.document.windows.len() >= 8 {
            return None;
        }
        let id = WorkspaceWindowId::new();
        let root = PaneId::new();
        let group = TabGroupId::new();
        self.document.windows.push(WorkspaceWindow {
            id,
            root,
            focused_group: group,
        });
        self.document.panes.push(WorkspacePane {
            id: root,
            content: WorkspacePaneContent::Tabs { group_id: group },
        });
        self.document.tab_groups.push(WorkspaceTabGroup {
            id: group,
            tabs: vec![],
            active_tab: None,
        });
        Some(id)
    }
    pub fn move_to_window(&mut self, tab: TabId, target: WorkspaceWindowId) -> bool {
        let Some(group) = self
            .document
            .windows
            .iter()
            .find(|w| w.id == target)
            .map(|w| w.focused_group)
        else {
            return false;
        };
        if self.pending_launch.is_some()
            || self.window_of_tab(tab).is_none()
            || self.window_of_tab(tab) == Some(target)
        {
            return false;
        }
        let before = self.document.clone();
        self.move_to_group(tab, group);
        if cshell_application::validate_workspace(&self.document).is_err() {
            self.document = before;
            return false;
        }
        true
    }
    pub fn close_window(&mut self, id: WorkspaceWindowId) -> bool {
        if self.document.windows.len() <= 1 || !self.document.windows.iter().any(|w| w.id == id) {
            return false;
        }
        let groups = Self::groups_in(&self.document, id);
        let tabs: Vec<_> = self
            .document
            .tab_groups
            .iter()
            .filter(|g| groups.contains(&g.id))
            .flat_map(|g| g.tabs.iter().copied())
            .collect();
        let root = self
            .document
            .windows
            .iter()
            .find(|w| w.id == id)
            .map(|w| w.root)
            .unwrap_or_else(PaneId::new);
        let mut panes = vec![root];
        let mut index = 0;
        while index < panes.len() {
            if let Some(WorkspacePane {
                content: WorkspacePaneContent::Split { first, second, .. },
                ..
            }) = self.document.panes.iter().find(|p| p.id == panes[index])
            {
                panes.extend([*first, *second]);
            }
            index += 1;
        }
        self.document.windows.retain(|w| w.id != id);
        if self.window_id == id {
            self.window_id = self.document.windows[0].id;
        }
        self.document.panes.retain(|p| !panes.contains(&p.id));
        self.document.tab_groups.retain(|g| !groups.contains(&g.id));
        self.document.bindings.retain(|b| !tabs.contains(&b.tab_id));
        self.sessions.retain(|tab, _| !tabs.contains(tab));
        if self.pending_launch.is_some_and(|tab| tabs.contains(&tab)) {
            self.pending_launch = None;
        }
        true
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
        let focused = self.focused_group();
        self.document
            .tab_groups
            .iter_mut()
            .find(|g| g.id == focused)?
            .tabs
            .push(id);
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
        } else if let Some((&id, _)) = self.sessions.iter().find(|(id, value)| {
            **value == session && self.window_of_tab(**id) == Some(self.window_id)
        }) {
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
        if let Some(group) = self
            .document
            .tab_groups
            .iter_mut()
            .find(|g| g.tabs.contains(&id))
        {
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
            let group_id = group.id;
            self.collapse_empty(group_id);
        }
        self.sessions.remove(&id);
    }
    pub fn move_tab(&mut self, id: TabId, delta: isize) {
        let Some(group) = self
            .document
            .tab_groups
            .iter_mut()
            .find(|g| g.tabs.contains(&id))
        else {
            return;
        };
        let tabs = &mut group.tabs;
        if let Some(index) = tabs.iter().position(|tab| *tab == id) {
            let target = index
                .saturating_add_signed(delta)
                .min(tabs.len().saturating_sub(1));
            tabs.swap(index, target);
        }
    }
    pub fn split(&mut self, axis: SplitAxis) -> Option<TabId> {
        if Self::groups_in(&self.document, self.window_id).len() >= 8
            || self.document.bindings.len() >= cshell_application::MAX_WORKSPACE_TABS
        {
            return None;
        }
        let group = self.focused_group();
        let leaf = self.document.panes.iter().position(
            |p| matches!(p.content, WorkspacePaneContent::Tabs { group_id } if group_id == group),
        )?;
        let before = self.document.clone();
        let binding = self
            .active()
            .and_then(|tab| self.document.bindings.iter().find(|b| b.tab_id == tab))
            .cloned();
        let first = PaneId::new();
        let second = PaneId::new();
        let new_group = TabGroupId::new();
        self.document.panes[leaf].content = WorkspacePaneContent::Split {
            axis,
            ratio_permille: 500,
            first,
            second,
        };
        self.document.panes.push(WorkspacePane {
            id: first,
            content: WorkspacePaneContent::Tabs { group_id: group },
        });
        self.document.panes.push(WorkspacePane {
            id: second,
            content: WorkspacePaneContent::Tabs {
                group_id: new_group,
            },
        });
        self.document.tab_groups.push(WorkspaceTabGroup {
            id: new_group,
            tabs: vec![],
            active_tab: None,
        });
        self.document
            .windows
            .iter_mut()
            .find(|w| w.id == self.window_id)?
            .focused_group = new_group;
        let tab = self.add(
            binding.as_ref().and_then(|b| b.profile_id),
            binding.map_or("Local shell".into(), |b| b.title),
        );
        if cshell_application::validate_workspace(&self.document).is_err() {
            self.document = before;
            return None;
        }
        tab
    }
    pub fn set_ratio(&mut self, pane: PaneId, ratio: u16) {
        if let Some(WorkspacePane {
            content: WorkspacePaneContent::Split { ratio_permille, .. },
            ..
        }) = self.document.panes.iter_mut().find(|p| p.id == pane)
        {
            *ratio_permille = ratio.clamp(100, 900);
        }
    }
    pub fn move_to_group(&mut self, tab: TabId, target: TabGroupId) {
        if !self.document.tab_groups.iter().any(|g| g.id == target) {
            return;
        }
        let Some(source) = self
            .document
            .tab_groups
            .iter()
            .find(|g| g.tabs.contains(&tab))
            .map(|g| g.id)
        else {
            return;
        };
        if source == target {
            return;
        }
        let target_window = self
            .document
            .windows
            .iter()
            .find(|w| Self::groups_in(&self.document, w.id).contains(&target))
            .map(|w| w.id);
        for group in &mut self.document.tab_groups {
            if group.id == source {
                group.tabs.retain(|id| *id != tab);
                if group.active_tab == Some(tab) {
                    group.active_tab = group.tabs.last().copied();
                }
            }
            if group.id == target {
                group.tabs.push(tab);
                group.active_tab = Some(tab);
            }
        }
        if let Some(window) = self
            .document
            .windows
            .iter_mut()
            .find(|w| Some(w.id) == target_window)
        {
            window.focused_group = target;
        }
        self.collapse_empty(source);
    }
    fn collapse_empty(&mut self, group: TabGroupId) {
        if self.document.tab_groups.len() == 1
            || !self
                .document
                .tab_groups
                .iter()
                .any(|g| g.id == group && g.tabs.is_empty())
        {
            return;
        }
        let Some(leaf) = self.document.panes.iter().find(|p| matches!(p.content, WorkspacePaneContent::Tabs { group_id } if group_id == group)).map(|p| p.id) else { return; };
        let Some((parent, sibling)) =
            self.document
                .panes
                .iter()
                .enumerate()
                .find_map(|(index, pane)| match pane.content {
                    WorkspacePaneContent::Split { first, second, .. } if first == leaf => {
                        Some((index, second))
                    }
                    WorkspacePaneContent::Split { first, second, .. } if second == leaf => {
                        Some((index, first))
                    }
                    _ => None,
                })
        else {
            return;
        };
        let Some(content) = self
            .document
            .panes
            .iter()
            .find(|p| p.id == sibling)
            .map(|p| p.content.clone())
        else {
            return;
        };
        self.document.panes[parent].content = content;
        self.document
            .panes
            .retain(|p| p.id != leaf && p.id != sibling);
        self.document.tab_groups.retain(|g| g.id != group);
        if let Some(id) = self
            .document
            .windows
            .iter()
            .find(|w| w.focused_group == group)
            .map(|w| w.id)
        {
            let remaining = Self::groups_in(&self.document, id);
            if let Some(focus) = remaining.first()
                && let Some(window) = self.document.windows.iter_mut().find(|w| w.id == id)
            {
                window.focused_group = *focus;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_move_live_bindings_without_persisting_runtime_sessions() {
        let mut workspace = DesktopWorkspace::default();
        let first = workspace
            .add(None, "First".into())
            .unwrap_or_else(|| panic!("first"));
        let second = workspace
            .add(None, "Second".into())
            .unwrap_or_else(|| panic!("second"));
        let session = SessionId::new();
        workspace.sessions.insert(second, session);
        let new_window = workspace.new_window().unwrap_or_else(|| panic!("window"));
        assert!(workspace.move_to_window(second, new_window));
        assert_eq!(workspace.window_of_tab(second), Some(new_window));
        assert_eq!(workspace.sessions.get(&second), Some(&session));
        assert_eq!(workspace.active(), Some(first));
        assert_eq!(workspace.visible_tabs(), vec![first]);
        workspace.window_id = new_window;
        assert_eq!(workspace.active(), Some(second));
        assert_eq!(workspace.visible_tabs(), vec![second]);
        assert!(cshell_application::validate_workspace(&workspace.document).is_ok());
        let restored =
            DesktopWorkspace::restore(workspace.document.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert!(restored.sessions.is_empty());
        assert_eq!(restored.document.windows.len(), 2);
        assert!(workspace.close_window(new_window));
        assert_eq!(workspace.window_id, workspace.document.windows[0].id);
        assert!(!workspace.sessions.contains_key(&second));
        assert!(cshell_application::validate_workspace(&workspace.document).is_ok());
    }

    #[test]
    fn a_window_does_not_claim_another_windows_session_and_pending_tab_cannot_move() {
        let mut workspace = DesktopWorkspace::default();
        let root = workspace.window_id;
        let tab = workspace
            .add(None, "Live".into())
            .unwrap_or_else(|| panic!("tab"));
        let session = SessionId::new();
        workspace.sessions.insert(tab, session);
        let other = workspace.new_window().unwrap_or_else(|| panic!("window"));
        workspace.pending_launch = Some(tab);
        let before = workspace.document.clone();
        assert!(!workspace.move_to_window(tab, other));
        assert_eq!(workspace.document, before);
        workspace.pending_launch = None;
        assert!(workspace.move_to_window(tab, other));
        assert_eq!(workspace.window_id, root);
        let cloned = workspace
            .observe(session, None, "Another view".into())
            .unwrap_or_else(|| panic!("clone"));
        assert_ne!(cloned, tab);
        assert_eq!(workspace.window_of_tab(cloned), Some(root));
        assert_eq!(workspace.sessions.get(&cloned), Some(&session));
        assert!(cshell_application::validate_workspace(&workspace.document).is_ok());
    }

    #[test]
    fn nested_splits_move_tabs_and_collapse_empty_groups_without_losing_bindings() {
        let mut workspace = DesktopWorkspace::default();
        let first = workspace
            .add(Some(ProfileId::new()), "First".into())
            .unwrap_or_else(|| panic!("tab"));
        let hidden = workspace
            .add(None, "Hidden".into())
            .unwrap_or_else(|| panic!("tab"));
        workspace.select(first);
        let second = workspace
            .split(SplitAxis::Horizontal)
            .unwrap_or_else(|| panic!("split"));
        let third = workspace
            .split(SplitAxis::Vertical)
            .unwrap_or_else(|| panic!("split"));
        assert_eq!(workspace.visible_tabs().len(), 3);
        assert!(!workspace.sessions.contains_key(&second));
        assert!(!workspace.sessions.contains_key(&third));
        let target = workspace.focused_group();
        workspace.move_to_group(hidden, target);
        assert_eq!(workspace.active(), Some(hidden));
        workspace.remove(third);
        workspace.remove(second);
        assert_eq!(workspace.document.tab_groups.len(), 2);
        workspace.remove(hidden);
        assert_eq!(workspace.document.tab_groups.len(), 1);
        assert_eq!(workspace.active(), Some(first));
        assert_eq!(workspace.document.panes.len(), 1);
        assert_eq!(workspace.document.bindings.len(), 1);
        assert!(cshell_application::validate_workspace(&workspace.document).is_ok());
        let restored =
            DesktopWorkspace::restore(workspace.document.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(restored.active(), Some(first));
        assert!(restored.sessions.is_empty());
    }

    #[test]
    fn split_limit_and_ratio_changes_leave_a_valid_versioned_layout() {
        let mut workspace = DesktopWorkspace::default();
        workspace.add(None, "Root".into());
        for _ in 0..7 {
            assert!(workspace.split(SplitAxis::Horizontal).is_some());
        }
        let before = workspace.document.clone();
        assert!(workspace.split(SplitAxis::Vertical).is_none());
        assert_eq!(workspace.document, before);
        let root = workspace.document.windows[0].root;
        workspace.set_ratio(root, 0);
        assert!(matches!(
            workspace
                .document
                .panes
                .iter()
                .find(|p| p.id == root)
                .map(|p| &p.content),
            Some(WorkspacePaneContent::Split {
                ratio_permille: 100,
                ..
            })
        ));
        assert!(cshell_application::validate_workspace(&workspace.document).is_ok());
        let restored =
            DesktopWorkspace::restore(workspace.document.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(restored.document, workspace.document);
        assert!(restored.sessions.is_empty());
    }

    #[test]
    fn full_tab_capacity_refuses_split_without_mutating_layout() {
        let mut workspace = DesktopWorkspace::default();
        for _ in 0..cshell_application::MAX_WORKSPACE_TABS {
            assert!(workspace.add(None, "Tab".into()).is_some());
        }
        let before = workspace.document.clone();
        assert!(workspace.split(SplitAxis::Horizontal).is_none());
        assert_eq!(workspace.document, before);
    }

    #[test]
    fn split_layout_restores_without_live_sessions() {
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
        let restored = DesktopWorkspace::restore(doc.clone()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(restored.document, doc);
        assert!(restored.sessions.is_empty());
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
