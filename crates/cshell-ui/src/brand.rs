//! Code-native master mark: graphite tile, C-shaped frame, terminal prompt.
pub fn paint(painter: &egui::Painter, rect: egui::Rect) {
    use egui::{Color32, Stroke, pos2};
    let scale = rect.width().min(rect.height()) / 64.0;
    let point = |x: f32, y: f32| pos2(rect.left() + x * scale, rect.top() + y * scale);
    painter.rect_filled(rect, 14.0 * scale, Color32::from_rgb(18, 28, 37));
    let green = Color32::from_rgb(86, 201, 182);
    painter.add(egui::Shape::line(
        vec![
            point(44.0, 16.0),
            point(24.0, 16.0),
            point(19.0, 18.0),
            point(16.0, 24.0),
            point(16.0, 40.0),
            point(19.0, 46.0),
            point(24.0, 48.0),
            point(44.0, 48.0),
        ],
        Stroke::new(6.0 * scale, green),
    ));
    painter.add(egui::Shape::line(
        vec![point(29.0, 25.0), point(37.0, 32.0), point(29.0, 39.0)],
        Stroke::new(4.5 * scale, Color32::from_rgb(240, 245, 247)),
    ));
    painter.line_segment(
        [point(42.0, 40.0), point(49.0, 40.0)],
        Stroke::new(4.0 * scale, green),
    );
}
