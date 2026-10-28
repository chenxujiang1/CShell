use crate::WorkbenchViewModel;

/// Presentation preferences only. Does not contain tabs, sessions or credentials.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Layout {
    pub sidebar_hidden: bool,
    pub sidebar_width: Option<f32>,
    pub toolbar_hidden: bool,
}
impl Layout {
    pub fn capture(model: &WorkbenchViewModel) -> Self {
        Self {
            sidebar_hidden: model.sidebar_hidden,
            sidebar_width: model.sidebar_width,
            toolbar_hidden: model.toolbar_hidden,
        }
        .normalized()
    }
    pub fn normalized(mut self) -> Self {
        self.sidebar_width = self
            .sidebar_width
            .filter(|w| w.is_finite())
            .map(|w| w.clamp(120.0, 720.0));
        self
    }
    pub fn apply(&self, model: &mut WorkbenchViewModel) {
        let value = self.clone().normalized();
        model.sidebar_hidden = value.sidebar_hidden;
        model.sidebar_width = value.sidebar_width;
        model.toolbar_hidden = value.toolbar_hidden;
    }
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        ui.strong("工作台布局");
        let mut sidebar = !self.sidebar_hidden;
        let mut toolbar = !self.toolbar_hidden;
        if ui.checkbox(&mut sidebar, "显示会话管理器").changed() {
            self.sidebar_hidden = !sidebar;
        }
        if ui.checkbox(&mut toolbar, "显示连接工具栏").changed() {
            self.toolbar_hidden = !toolbar;
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("标准布局").clicked() {
                *self = Self::default();
            }
            if ui.button("专注终端").clicked() {
                self.sidebar_hidden = true;
                self.toolbar_hidden = true;
            }
            if ui.button("侧栏自动宽度").clicked() {
                self.sidebar_width = None;
            }
        });
        ui.weak("即时应用并记住布局；新窗口沿用此偏好。已有窗口可各自调整。菜单“查看”可重新展开工具栏和侧栏。");
    }
}
