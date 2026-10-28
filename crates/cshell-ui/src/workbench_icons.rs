//! Small line icons drawn in logical pixels; no platform font dependencies.
use egui::{Color32, Painter, Pos2, Rect, Stroke, vec2};

#[derive(Clone, Copy, Debug)]
pub enum Icon {
    Add,
    Connect,
    Disconnect,
    Reconnect,
    Close,
    Terminal,
    Sessions,
    Key,
    Folder,
    Search,
    Upload,
    Download,
    Back,
    Forward,
    Up,
    Refresh,
    Edit,
    Server,
    Computer,
    Pause,
    Play,
}

pub fn paint(painter: &Painter, center: Pos2, icon: Icon, color: Color32) {
    let s = Stroke::new(1.4, color);
    let p = |x, y| center + vec2(x, y);
    let line = |a: (f32, f32), b: (f32, f32)| {
        painter.line_segment([p(a.0, a.1), p(b.0, b.1)], s);
    };
    let box_outline = || {
        painter.rect_stroke(
            Rect::from_center_size(center, vec2(16.0, 13.0)),
            2.0,
            s,
            egui::StrokeKind::Inside,
        );
    };
    match icon {
        Icon::Key => {
            painter.circle_stroke(p(-3.0, -2.0), 4.0, s);
            line((0.0, 1.0), (6.0, 7.0));
            line((3.0, 4.0), (5.0, 2.0));
            line((5.0, 6.0), (7.0, 4.0));
        }
        Icon::Add => {
            line((-5.0, 0.0), (5.0, 0.0));
            line((0.0, -5.0), (0.0, 5.0));
        }
        Icon::Close => {
            line((-3.5, -3.5), (3.5, 3.5));
            line((3.5, -3.5), (-3.5, 3.5));
        }
        Icon::Terminal => {
            box_outline();
            line((-4.0, -2.5), (-1.0, 0.0));
            line((-1.0, 0.0), (-4.0, 2.5));
            line((1.0, 3.0), (4.0, 3.0));
        }
        Icon::Connect => {
            line((-7.0, 0.0), (6.0, 0.0));
            line((2.0, -4.0), (6.0, 0.0));
            line((6.0, 0.0), (2.0, 4.0));
            line((-4.0, -5.0), (-4.0, -2.0));
            line((-4.0, 2.0), (-4.0, 5.0));
        }
        Icon::Disconnect => {
            line((-7.0, 0.0), (-2.0, 0.0));
            line((2.0, 0.0), (7.0, 0.0));
            line((-2.0, -4.0), (-2.0, 4.0));
            line((2.0, -4.0), (2.0, 4.0));
            line((-6.0, 6.0), (6.0, -6.0));
        }
        Icon::Reconnect | Icon::Refresh => {
            painter.add(egui::Shape::line(
                (0..=20)
                    .map(|i| {
                        let angle = 0.5 + i as f32 * 5.0 / 20.0;
                        p(angle.cos() * 6.0, angle.sin() * 6.0)
                    })
                    .collect(),
                s,
            ));
            line((4.2, -4.3), (0.5, -4.3));
            line((4.2, -4.3), (4.2, -0.6));
        }
        Icon::Sessions => {
            painter.rect_stroke(
                Rect::from_min_max(p(-7.0, -6.0), p(7.0, 6.0)),
                2.0,
                s,
                egui::StrokeKind::Inside,
            );
            line((-3.0, -5.0), (-3.0, 5.0));
            line((0.0, -2.0), (4.0, -2.0));
            line((0.0, 2.0), (4.0, 2.0));
        }
        Icon::Folder => {
            painter.add(egui::Shape::closed_line(
                vec![
                    p(-7.0, -4.0),
                    p(-2.0, -4.0),
                    p(0.0, -2.0),
                    p(7.0, -2.0),
                    p(7.0, 6.0),
                    p(-7.0, 6.0),
                ],
                s,
            ));
        }
        Icon::Upload | Icon::Forward => {
            line((-6.0, 0.0), (6.0, 0.0));
            line((2.0, -4.0), (6.0, 0.0));
            line((2.0, 4.0), (6.0, 0.0));
        }
        Icon::Download | Icon::Back => {
            line((6.0, 0.0), (-6.0, 0.0));
            line((-2.0, -4.0), (-6.0, 0.0));
            line((-2.0, 4.0), (-6.0, 0.0));
        }
        Icon::Up => {
            line((0.0, 6.0), (0.0, -6.0));
            line((-4.0, -2.0), (0.0, -6.0));
            line((4.0, -2.0), (0.0, -6.0));
        }
        Icon::Server => {
            for y in [-4.0, 4.0] {
                painter.rect_stroke(
                    Rect::from_center_size(p(0.0, y), vec2(15.0, 6.0)),
                    1.0,
                    s,
                    egui::StrokeKind::Inside,
                );
                painter.circle_filled(p(4.0, y), 1.0, color);
            }
        }
        Icon::Computer => {
            box_outline();
            line((0.0, 6.0), (0.0, 9.0));
            line((-4.0, 9.0), (4.0, 9.0));
        }
        Icon::Edit => {
            line((-5.0, 5.0), (4.0, -4.0));
            line((-5.0, 5.0), (-6.0, 7.0));
            line((-6.0, 7.0), (-3.0, 6.0));
            line((2.0, -6.0), (6.0, -2.0));
        }
        Icon::Pause => {
            line((-3.0, -5.0), (-3.0, 5.0));
            line((3.0, -5.0), (3.0, 5.0));
        }
        Icon::Play => {
            painter.add(egui::Shape::closed_line(
                vec![p(-4.0, -6.0), p(5.0, 0.0), p(-4.0, 6.0)],
                s,
            ));
        }
        Icon::Search => {
            painter.circle_stroke(p(-1.5, -1.5), 4.5, s);
            line((2.0, 2.0), (6.0, 6.0));
        }
    }
}
