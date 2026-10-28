use super::*;

/// Paths are the identity; ranges always follow the current visible sort order.
#[derive(Default)]
pub(super) struct Selection {
    directory: Option<String>,
    pub(super) paths: BTreeSet<String>,
    anchor: Option<String>,
    pub(super) cursor:Option<String>,
    pub(super) scroll_requested:bool,
}
impl Selection {
    pub(super) fn sync(
        &mut self,
        directory: Option<&str>,
        entries: &[cshell_ipc::SftpDirectoryEntry],
    ) {
        // Loading temporarily clears the listing. Keep selection until its successful replacement.
        let Some(directory) = directory else {
            return;
        };
        if self.directory.as_deref() != Some(directory) {
            self.paths.clear();
            self.anchor = None;
            self.cursor=None;
            self.directory = Some(directory.into());
        }
        self.paths
            .retain(|path| entries.iter().any(|entry| entry.path == *path));
        if self.cursor.as_ref().is_some_and(|p|!entries.iter().any(|e|&e.path==p)){self.cursor=None;}
        if self
            .anchor
            .as_ref()
            .is_some_and(|path| !entries.iter().any(|entry| entry.path == *path))
        {
            self.anchor = None;
        }
    }
    pub(super) fn click(
        &mut self,
        path: &str,
        order: &[&cshell_ipc::SftpDirectoryEntry],
        modifiers: egui::Modifiers,
    ) {
        self.cursor=Some(path.into());
        let additive = modifiers.ctrl || modifiers.command;
        if modifiers.shift {
            let a = self
                .anchor
                .as_ref()
                .and_then(|anchor| order.iter().position(|entry| entry.path == *anchor));
            let b = order.iter().position(|entry| entry.path == path);
            if !additive {
                self.paths.clear();
            }
            if let (Some(a), Some(b)) = (a, b) {
                self.paths.extend(
                    order[a.min(b)..=a.max(b)]
                        .iter()
                        .map(|entry| entry.path.clone()),
                );
            } else {
                self.paths.insert(path.into());
                self.anchor = Some(path.into());
            }
        } else {
            if additive {
                if !self.paths.remove(path) {
                    self.paths.insert(path.into());
                }
            } else {
                self.paths.clear();
                self.paths.insert(path.into());
            }
            self.anchor = Some(path.into());
        }
    }
    pub(super) fn context(&mut self, path: &str) {
        self.cursor=Some(path.into());
        if !self.paths.contains(path) {
            self.paths.clear();
            self.paths.insert(path.into());
            self.anchor = Some(path.into());
        }
    }
    pub(super) fn move_cursor(&mut self,order:&[&cshell_ipc::SftpDirectoryEntry],delta:i32,absolute:bool,modifiers:egui::Modifiers)->Option<String> {
        if order.is_empty(){return None;}
        let current=self.cursor.as_ref().and_then(|p|order.iter().position(|e|&e.path==p));
        let index=if absolute {if delta<0{0}else{order.len()-1}}else{current.map_or(if delta<0{order.len()-1}else{0},|i|(i as i64+i64::from(delta)).clamp(0,(order.len()-1) as i64) as usize)};
        let path=order[index].path.clone();
        self.click(&path,order,modifiers);self.scroll_requested=true;Some(path)
    }
}
