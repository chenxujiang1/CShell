//! Explicit profile metadata; never inferred from hostnames or terminal output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Environment {
    Production,
    Staging,
    Test,
    Development,
}
impl Environment {
    pub const ALL: [Self; 4] = [
        Self::Production,
        Self::Staging,
        Self::Test,
        Self::Development,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Production => "生产",
            Self::Staging => "预发布",
            Self::Test => "测试",
            Self::Development => "开发",
        }
    }
    pub fn tag(self) -> &'static str {
        match self {
            Self::Production => "env:production",
            Self::Staging => "env:staging",
            Self::Test => "env:test",
            Self::Development => "env:development",
        }
    }
    pub fn color(self) -> egui::Color32 {
        match self {
            Self::Production => crate::theme::DANGER,
            Self::Staging => crate::theme::WARNING,
            Self::Test => crate::theme::ACCENT,
            Self::Development => crate::theme::SUCCESS,
        }
    }
    pub fn from_tags<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        let mut found = None;
        for tag in tags {
            if let Some(env) = Self::ALL.into_iter().find(|env| env.tag() == tag.trim()) {
                if found.is_some_and(|old| old != env) {
                    return None;
                }
                found = Some(env);
            }
        }
        found
    }
}
pub fn selector(ui: &mut egui::Ui, tags: &mut String) {
    let mut selected = Environment::from_tags(tags.split(','));
    let mut touched = false;
    ui.label("环境标记（可选）");
    egui::ComboBox::from_id_salt("profile-environment")
        .selected_text(selected.map_or("不标记", Environment::label))
        .show_ui(ui, |ui| {
            touched |= ui.selectable_value(&mut selected, None, "不标记").clicked();
            for env in Environment::ALL {
                touched |= ui
                    .selectable_value(&mut selected, Some(env), env.label())
                    .clicked();
            }
        });
    if touched {
        replace_marker(tags, selected);
    }
    ui.weak("保存后显示在 SSH 主机身份栏；使用 env: 标签，可随配置导入导出。");
}

fn replace_marker(tags: &mut String, selected: Option<Environment>) {
    let mut others: Vec<_> = tags
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty() && !Environment::ALL.iter().any(|env| env.tag() == *tag))
        .map(str::to_owned)
        .collect();
    if let Some(env) = selected {
        others.push(env.tag().to_owned());
    }
    *tags = others.join(", ");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn markers_are_explicit_unique_and_preserve_other_profile_tags() {
        assert_eq!(Environment::from_tags(["production", "prod.example"]), None);
        assert_eq!(
            Environment::from_tags(["env:production", "env:development"]),
            None
        );
        let mut tags = "linux, env:production, env:development, team:ops, env:custom".to_owned();
        replace_marker(&mut tags, Some(Environment::Staging));
        assert_eq!(
            Environment::from_tags(tags.split(',')),
            Some(Environment::Staging)
        );
        assert_eq!(tags, "linux, team:ops, env:custom, env:staging");
        replace_marker(&mut tags, None);
        assert_eq!(tags, "linux, team:ops, env:custom");
    }
}
