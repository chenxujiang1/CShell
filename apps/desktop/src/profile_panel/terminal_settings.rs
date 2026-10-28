use super::*;

fn resolved(
    catalog: &DesktopProfileCatalog,
    profile: &ProfileRecord,
) -> Option<cshell_domain::ResolvedTerminalSettings> {
    let mut records = catalog.profiles.clone();
    if let Some(record) = records.iter_mut().find(|p| p.id == profile.id) {
        *record = profile.clone();
    } else {
        records.push(profile.clone());
    }
    let snapshot = cshell_application::CatalogSnapshot {
        revision: catalog.revision,
        defaults: catalog.defaults.clone(),
        folders: catalog.folders.clone(),
        profiles: records,
        ssh_connections: catalog.ssh_connections.clone(),
        local_connections: catalog.local_connections.clone(),
        themes: catalog.themes.clone(),
    };
    cshell_application::ProfileCatalog::from_snapshot(snapshot)
        .ok()?
        .resolve(profile.id)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn draft_preview_resolves_global_group_and_profile_without_saving() {
        let mut catalog = DesktopProfileCatalog {
            revision: 1,
            defaults: Default::default(),
            folders: vec![],
            profiles: vec![],
            ssh_connections: vec![],
            local_connections: vec![],
            themes: vec![],
        };
        let folder = ProfileFolder {
            id: FolderId::new(),
            name: "Linux".into(),
            parent_id: None,
            terminal: TerminalOverrides {
                logging: Some(true),
                ..Default::default()
            },
        };
        let mut draft = ProfileRecord {
            id: ProfileId::new(),
            name: "Preview".into(),
            kind: ProfileKind::Ssh,
            folder_id: Some(folder.id),
            tags: Default::default(),
            favorite: false,
            terminal: Default::default(),
        };
        catalog.folders.push(folder.clone());
        let settings = resolved(&catalog, &draft).unwrap();
        assert_eq!(settings.theme.source, cshell_domain::SettingSource::Global);
        assert_eq!(
            settings.logging.source,
            cshell_domain::SettingSource::Folder(folder.id)
        );
        assert!(settings.logging.value);
        draft.terminal.logging = Some(false);
        let settings = resolved(&catalog, &draft).unwrap();
        assert_eq!(
            settings.logging.source,
            cshell_domain::SettingSource::Profile(draft.id)
        );
        assert!(!settings.logging.value);
        draft.terminal = Default::default();
        assert!(resolved(&catalog, &draft).unwrap().logging.value);
        assert!(catalog.profiles.is_empty());
        assert!(catalog.folders[0].terminal.logging.unwrap());
    }
}
fn source(source: cshell_domain::SettingSource, catalog: &DesktopProfileCatalog) -> String {
    match source {
        cshell_domain::SettingSource::Global => "全局默认".into(),
        cshell_domain::SettingSource::Folder(id) => format!(
            "分组 / {}",
            catalog
                .folders
                .iter()
                .find(|f| f.id == id)
                .map_or("未命名", |f| f.name.as_str())
        ),
        cshell_domain::SettingSource::Profile(_) => "本会话覆盖".into(),
        _ => "连接策略".into(),
    }
}
pub(super) fn draw(ui: &mut egui::Ui, catalog: &DesktopProfileCatalog, profile: &ProfileRecord) {
    ui.separator();
    ui.strong("配置继承预览");
    let Some(settings) = resolved(catalog, profile) else {
        ui.weak("补全有效的配置后显示继承结果。");
        return;
    };
    for (label, value, origin) in [
        (
            "主题",
            settings.theme.value.clone(),
            source(settings.theme.source, catalog),
        ),
        (
            "终端类型",
            settings.terminal_type.value.clone(),
            source(settings.terminal_type.source, catalog),
        ),
        (
            "日志",
            if settings.logging.value {
                "开启"
            } else {
                "关闭"
            }
            .into(),
            source(settings.logging.source, catalog),
        ),
    ] {
        let line = format!("{label}：{value} · 来源：{origin}");
        ui.add(egui::Label::new(&line).truncate())
            .on_hover_text(line);
    }
    crate::terminal_appearance::preview(
        ui,
        &settings.theme.value,
        &catalog.themes,
        crate::terminal_appearance::font(ui.ctx()),
    );
    ui.weak("当前预览反映草稿，保存后应用主题；终端类型和日志对后续连接生效。连接策略或临时设置可能进一步覆盖。");
}
