use super::*;
use crate::sftp_settings::SyncPreset;
pub(super) struct Navigation {
    local: String,
    remote: String,
    phase: u8,
    started: std::time::Instant,
}
impl Panel {
    pub(super) fn advance_preset(
        &mut self,
        session: SessionId,
        view: &SftpView,
        command: &mut Option<SftpClientCommand>,
    ) {
        let Some(nav) = &mut self.preset_navigation else {
            return;
        };
        if nav.started.elapsed() > std::time::Duration::from_secs(15) {
            self.local_error =
                Some("方案目录尚未载入，请检查连接或重新选择方案；没有提交传输。".into());
            self.preset_navigation = None;
            return;
        }
        if command.is_some() {
            return;
        }
        if nav.phase == 0 && view.local_directory.as_ref() != Some(&nav.local) {
            *command = Some(SftpClientCommand::BrowseLocal(
                session,
                cshell_application::SftpBrowseOptions {
                    local: true,
                    path: nav.local.clone(),
                    links: cshell_application::SftpLinkPolicy::NoFollow,
                },
            ));
            nav.phase = 1;
            return;
        }
        if nav.phase <= 1 {
            if view.local_directory.as_ref() != Some(&nav.local) {
                return;
            }
            nav.phase = 2;
        }
        if nav.phase == 2 && view.remote_loaded_directory.as_ref() != Some(&nav.remote) {
            *command = Some(SftpClientCommand::BrowseManaged(
                session,
                cshell_application::SftpBrowseOptions {
                    local: false,
                    path: nav.remote.clone(),
                    links: cshell_application::SftpLinkPolicy::NoFollow,
                },
            ));
            nav.phase = 3;
            return;
        }
        if view.remote_loaded_directory.as_ref() == Some(&nav.remote) {
            self.preset_navigation = None;
            self.local_error = None;
        }
    }
    pub(super) fn preset_tools(&mut self, ui: &mut egui::Ui, view: &SftpView) {
        if !self.presets_loaded {
            self.presets_loaded = true;
            #[cfg(not(test))]
            {
                self.presets = crate::sftp_settings::sync_presets();
            }
        }
        let mut selected = None;
        let label = self
            .preset_id
            .and_then(|id| self.presets.iter().find(|p| p.id == id))
            .map_or("方案与规则".to_owned(), |p| {
                format!("方案：{}", p.name)
            });
        egui::containers::menu::MenuButton::new(label).config(egui::containers::menu::MenuConfig::new().close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)).ui(ui,|ui|{
            ui.set_width(340.0_f32.min(ui.ctx().content_rect().width()-50.0));
            ui.strong("当前主机的同步方案");
            egui::ComboBox::from_id_salt("sync-preset-list").selected_text("选择已保存方案…").width(280.0).show_ui(ui,|ui|{
                for preset in self.presets.iter().filter(|p|view.peer.as_ref()==Some(&p.peer)) {if ui.selectable_label(self.preset_id==Some(preset.id),&preset.name).clicked(){selected=Some(preset.clone());}}
            });
            ui.add(egui::TextEdit::singleline(&mut self.preset_name).hint_text("方案名称").char_limit(64));
            ui.label("排除规则（每行一项，区分大小写）");
            if ui.add(egui::TextEdit::multiline(&mut self.excludes_text).desired_rows(3).char_limit(2048).hint_text(".git/**\nnode_modules/**\n*.tmp")).changed(){self.id=None;self.selected.clear();}
            if ui.checkbox(&mut self.preserve_modified,"保留文件修改时间").changed(){self.id=None;self.selected.clear();}
            ui.weak("* 匹配名称，** 匹配多层目录；最多 16 项。加载方案只切换目录，传输仍需比较后提交。");
            let can_save=view.peer.is_some()&&view.sync_pending.is_none()&&self.preset_navigation.is_none();
            ui.horizontal(|ui|{
                if ui.add_enabled(can_save,egui::Button::new("保存方案")).clicked(){
                    if let (Some(peer),Some(local),Some(remote))=(&view.peer,&view.local_directory,&view.remote_loaded_directory){
                        let preset=SyncPreset{id:self.preset_id.unwrap_or_else(SessionId::new),name:self.preset_name.trim().into(),peer:peer.clone(),local:local.clone(),remote:remote.clone(),direction:self.direction.unwrap_or(SftpQueueDirection::Upload),excludes:self.rules(),compare_content:self.compare_content.unwrap_or(true),preserve_modified:self.preserve_modified};
                        let id=preset.id;
                        #[cfg(not(test))] let saved=crate::sftp_settings::save_sync_preset(preset);
                        #[cfg(test)] let saved=Ok::<_,String>(vec![preset]);
                        match saved{Ok(values)=>{self.presets=values;self.preset_id=Some(id);self.local_error=None;},Err(error)=>self.local_error=Some(error)}
                    }
                }
                if ui.add_enabled(self.preset_id.is_some()&&view.sync_pending.is_none(),egui::Button::new("删除方案")).clicked(){if let Some(id)=self.preset_id {
                    #[cfg(not(test))] let saved=crate::sftp_settings::delete_sync_preset(id);
                    #[cfg(test)] let saved=Ok::<_,String>(self.presets.iter().filter(|p|p.id!=id).cloned().collect::<Vec<_>>());
                    match saved{Ok(values)=>{self.presets=values;self.preset_id=None;},Err(error)=>self.local_error=Some(error)}
                }}
            });
        });
        if let Some(preset) = selected {
            self.preset_name = preset.name.clone();
            self.preset_id = Some(preset.id);
            self.direction = Some(preset.direction);
            self.compare_content = Some(preset.compare_content);
            self.preserve_modified = preset.preserve_modified;
            self.excludes_text = preset.excludes.join("\n");
            self.id = None;
            self.selected.clear();
            self.submitted = false;
            self.preset_navigation = Some(Navigation {
                local: preset.local,
                remote: preset.remote,
                phase: 0,
                started: std::time::Instant::now(),
            });
        }
    }
    pub(super) fn rules(&self) -> Vec<String> {
        self.excludes_text
            .lines()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preset_navigates_once_per_pane_without_starting_transfer_and_times_out() {
        let session = SessionId::new();
        let mut panel = Panel::default();
        let mut view = SftpView::default();
        panel.preset_navigation = Some(Navigation {
            local: "C:/work".into(),
            remote: "/srv/work".into(),
            phase: 0,
            started: std::time::Instant::now(),
        });
        let mut command = None;
        panel.advance_preset(session, &view, &mut command);
        assert!(matches!(command, Some(SftpClientCommand::BrowseLocal(..))));
        command = None;
        panel.advance_preset(session, &view, &mut command);
        assert!(command.is_none());
        view.local_directory = Some("C:/work".into());
        panel.advance_preset(session, &view, &mut command);
        assert!(matches!(
            command,
            Some(SftpClientCommand::BrowseManaged(..))
        ));
        command = None;
        panel.advance_preset(session, &view, &mut command);
        assert!(command.is_none());
        view.remote_loaded_directory = Some("/srv/work".into());
        panel.advance_preset(session, &view, &mut command);
        assert!(panel.preset_navigation.is_none() && command.is_none());
        panel.preset_navigation = Some(Navigation {
            local: "C:/missing".into(),
            remote: "/missing".into(),
            phase: 1,
            started: std::time::Instant::now() - std::time::Duration::from_secs(16),
        });
        panel.advance_preset(session, &view, &mut command);
        assert!(
            panel.preset_navigation.is_none() && panel.local_error.is_some() && command.is_none()
        );
    }
}
