//! Shared bounds for dialogs, fields and popup menus.

/// Utility rows share the same surface as the workbench and settings windows.
pub fn utility_bar() -> egui::Frame {
    egui::Frame::NONE
        .fill(crate::theme::PANEL)
        .stroke(egui::Stroke::new(1.0, crate::theme::LINE))
        .inner_margin(egui::Margin::symmetric(8, 2))
}

/// Page navigation has a restrained selection fill and a persistent underline.
pub fn tab<T: PartialEq>(
    ui: &mut egui::Ui,
    current: &mut T,
    value: T,
    label: &str,
) -> egui::Response {
    let selected = *current == value;
    let response = ui.selectable_value(current, value, label);
    if selected {
        ui.painter().line_segment(
            [
                response.rect.left_bottom() + egui::vec2(5.0, 0.0),
                response.rect.right_bottom() - egui::vec2(5.0, 0.0),
            ],
            egui::Stroke::new(1.5, crate::theme::ACCENT),
        );
    }
    response
}

/// Dialog content scrolls vertically while the window stays inside the viewport.
pub fn dialog<'a>(context: &egui::Context, title: impl Into<egui::WidgetText>) -> egui::Window<'a> {
    let bounds = context.content_rect().shrink(12.0);
    egui::Window::new(title)
        .constrain_to(bounds)
        .min_size(egui::vec2(320.0, 180.0).min(bounds.size()))
        .max_size(bounds.size())
        .collapsible(false)
        .vscroll(true)
}

/// A field occupies its own row; long labels never compete with the editor.
pub fn text(ui: &mut egui::Ui, label: &str, value: &mut String) -> egui::Response {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(ui.available_width()))
}

/// Keep a dropdown's label above its selected value so both fit a narrow form.
pub fn combo(ui: &mut egui::Ui, label: &str) -> egui::ComboBox {
    ui.label(label);
    egui::ComboBox::from_id_salt(("form-combo", label))
        .width(ui.available_width().clamp(1.0, 240.0))
        .truncate()
}

/// Bound the expanded menu separately from the single-line selected value.
pub fn menu(ui: &mut egui::Ui) {
    let width = (ui.ctx().content_rect().width() - 48.0).clamp(80.0, 360.0);
    ui.set_max_width(width);
    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Wrap);
    ui.spacing_mut().button_padding = egui::vec2(8.0, 5.0);
}

/// Menus use full-width rows so labels, hover feedback and hit targets align.
pub fn menu_item(ui: &mut egui::Ui, label: &str, enabled: bool) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        ui.add_sized([ui.available_width(), 28.0], egui::Button::new(label).right_text(()))
    }).inner
}

/// Keep long submenus reachable without increasing their parent window.
pub fn menu_body<R>(ui: &mut egui::Ui, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    menu(ui);
    let height = (ui.ctx().content_rect().height() - 80.0).max(60.0);
    egui::ScrollArea::vertical()
        .id_salt(ui.id().with("menu-scroll"))
        .max_height(height)
        .show(ui, contents)
        .inner
}
