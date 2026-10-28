use super::*;
pub(in crate::sftp_panel) struct LocalUi {
    directory: String,
    file: String,
    default_directory: String,
    default_status: Option<String>,
    loaded: Option<String>,
    history: Vec<String>,
    selection: selection::Selection,
    sort: Option<columns::SortState>,
    navigation: navigation::History,
    editing: bool,
    focus: bool,
}
impl SftpPanel {
    pub(in crate::sftp_panel) fn take_local_ui(&mut self) -> LocalUi {
        LocalUi {
            directory: std::mem::take(&mut self.local_directory),
            file: std::mem::take(&mut self.local_file),
            default_directory: std::mem::take(&mut self.default_local_directory),
            default_status: self.default_directory_status.take(),
            loaded: self.browser.local_loaded.take(),
            history: std::mem::take(&mut self.browser.local_history),
            selection: std::mem::take(&mut self.browser.selection[0]),
            sort: self.browser.sort[0].take(),
            navigation: std::mem::take(&mut self.browser.navigation.history[0]),
            editing: std::mem::take(&mut self.browser.navigation.editing[0]),
            focus: std::mem::take(&mut self.browser.navigation.focus[0]),
        }
    }
    pub(in crate::sftp_panel) fn restore_local_ui(&mut self, local: LocalUi) {
        self.local_directory = local.directory;
        self.local_file = local.file;
        self.default_local_directory = local.default_directory;
        self.default_directory_status = local.default_status;
        self.browser.local_loaded = local.loaded;
        self.browser.local_history = local.history;
        self.browser.selection[0] = local.selection;
        self.browser.sort[0] = local.sort;
        self.browser.navigation.history[0] = local.navigation;
        self.browser.navigation.editing[0] = local.editing;
        self.browser.navigation.focus[0] = local.focus;
    }
    pub(crate) fn carry_local_to(&mut self, target: &mut Self) {
        let common = self.take_local_ui();
        target.restore_local_ui(common);
        std::mem::swap(&mut self.layout, &mut target.layout);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn common_local_moves_while_each_hosts_remote_selection_and_page_stay_put() {
        let mut a = SftpPanel::default();
        let mut b = SftpPanel::default();
        a.local_directory = "local-first".into();
        a.local_file = "local-first/file".into();
        a.browser.local_history = vec!["local-first".into()];
        a.browser.selection[0].paths.insert(a.local_file.clone());
        a.remote_directory = "/host-a".into();
        a.page = 2;
        a.browser.selection[1]
            .paths
            .insert("/host-a/selected".into());
        b.remote_directory = "/host-b".into();
        b.page = 3;
        b.browser.selection[1]
            .paths
            .insert("/host-b/selected".into());
        a.carry_local_to(&mut b);
        assert_eq!(b.local_directory, "local-first");
        assert!(b.browser.selection[0].paths.contains("local-first/file"));
        assert_eq!(b.remote_directory, "/host-b");
        assert_eq!(b.page, 3);
        assert!(b.browser.selection[1].paths.contains("/host-b/selected"));
        b.local_directory = "local-second".into();
        b.browser.local_history.push("local-second".into());
        b.carry_local_to(&mut a);
        assert_eq!(a.local_directory, "local-second");
        assert_eq!(a.browser.local_history.len(), 2);
        assert_eq!(a.remote_directory, "/host-a");
        assert_eq!(a.page, 2);
        assert!(a.browser.selection[1].paths.contains("/host-a/selected"));
    }
}
