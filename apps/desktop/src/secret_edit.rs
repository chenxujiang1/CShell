//! Password input without egui TextEdit's plaintext undo history.
use zeroize::Zeroize;

#[derive(Clone, Copy, Default)]
struct Cursor {
    offset: usize,
    all: bool,
    first_visible: usize,
    last_activity: f64,
}

pub fn edit(
    ui: &mut egui::Ui,
    value: &mut String,
    id_source: &str,
    maximum: usize,
) -> egui::Response {
    let id = ui.make_persistent_id(id_source);
    // Allocate before receiving text so growing a password cannot abandon old bytes.
    if value.capacity() < maximum {
        let mut replacement = String::with_capacity(maximum);
        replacement.push_str(value);
        value.zeroize();
        *value = replacement;
    }
    let (_, rect) = ui.allocate_space(egui::vec2(
        ui.available_width().clamp(1.0, 360.0),
        ui.spacing().interact_size.y,
    ));
    let mut response = ui
        .interact(rect, id, egui::Sense::click())
        .on_hover_cursor(egui::CursorIcon::Text);
    let mut cursor = ui.data_mut(|data| data.get_temp::<Cursor>(id).unwrap_or_default());
    cursor.offset = cursor.offset.min(value.len());
    while !value.is_char_boundary(cursor.offset) {
        cursor.offset -= 1;
    }
    let font = egui::TextStyle::Body.resolve(ui.style());
    let bullet_width = ui
        .painter()
        .layout_no_wrap("•".into(), font.clone(), ui.visuals().text_color())
        .size()
        .x
        .max(1.0);
    if response.clicked() {
        response.request_focus();
        let column = response
            .interact_pointer_pos()
            .map_or(value.chars().count(), |p| {
                cursor.first_visible
                    + ((p.x - rect.min.x - 5.0).max(0.0) / bullet_width).round() as usize
            });
        cursor.offset = value
            .char_indices()
            .nth(column)
            .map_or(value.len(), |(offset, _)| offset);
        cursor.all = false;
        cursor.last_activity = ui.input(|input| input.time);
    }
    if response.has_focus() && ui.is_enabled() {
        // Defer context writes until the input read guard has been released.
        let mut surrender_focus=false;
        // Events are borrowed: no additional plaintext event copies or persistent history.
        ui.input(|input| {
            for event in &input.events {
                match event {
                    egui::Event::Text(text)
                    | egui::Event::Paste(text)
                    | egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                        if text.chars().any(char::is_control) {
                            continue;
                        }
                        let remaining =
                            maximum.saturating_sub(if cursor.all { 0 } else { value.len() });
                        if text.len() > remaining {
                            continue;
                        }
                        if cursor.all {
                            value.zeroize();
                            cursor.offset = 0;
                            cursor.all = false;
                        }
                        value.insert_str(cursor.offset, text);
                        cursor.offset += text.len();
                        response.mark_changed();
                    }
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => match key {
                        egui::Key::A if modifiers.command || modifiers.ctrl => {
                            cursor.all = true;
                        }
                        egui::Key::Backspace | egui::Key::Delete if cursor.all => {
                            value.zeroize();
                            cursor.offset = 0;
                            cursor.all = false;
                            response.mark_changed();
                        }
                        egui::Key::Backspace if cursor.offset > 0 => {
                            let previous = value[..cursor.offset]
                                .char_indices()
                                .next_back()
                                .map_or(0, |(index, _)| index);
                            value.drain(previous..cursor.offset);
                            cursor.offset = previous;
                            response.mark_changed();
                        }
                        egui::Key::Delete if cursor.offset < value.len() => {
                            value.remove(cursor.offset);
                            response.mark_changed();
                        }
                        egui::Key::ArrowLeft => {
                            cursor.offset = if cursor.all {
                                0
                            } else {
                                value[..cursor.offset]
                                    .char_indices()
                                    .next_back()
                                    .map_or(0, |(index, _)| index)
                            };
                            cursor.all = false;
                        }
                        egui::Key::ArrowRight => {
                            cursor.offset = if cursor.all {
                                value.len()
                            } else {
                                cursor.offset
                                    + value[cursor.offset..]
                                        .chars()
                                        .next()
                                        .map_or(0, char::len_utf8)
                            };
                            cursor.all = false;
                        }
                        egui::Key::Home => {
                            cursor.offset = 0;
                            cursor.all = false;
                        }
                        egui::Key::End => {
                            cursor.offset = value.len();
                            cursor.all = false;
                        }
                        egui::Key::Escape | egui::Key::Enter => surrender_focus=true,
                        _ => {}
                    },
                    _ => {}
                }
            }
        });
        if surrender_focus {response.surrender_focus();}
        if response.changed()
            || ui.input(|input| {
                input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Key { pressed: true, .. }))
            })
        {
            cursor.last_activity = ui.input(|input| input.time);
        }
        ui.input_mut(|input| {
            for event in input.events.iter_mut().chain(input.raw.events.iter_mut()) {
                match event {
                    egui::Event::Text(text)
                    | egui::Event::Paste(text)
                    | egui::Event::Ime(
                        egui::ImeEvent::Commit(text) | egui::ImeEvent::Preedit { text, .. },
                    ) => text.zeroize(),
                    _ => {}
                }
            }
        });
    }
    let visuals = ui.style().interact(&response);
    ui.painter().rect(
        rect,
        visuals.corner_radius,
        ui.visuals().extreme_bg_color,
        if response.has_focus() {
            ui.visuals().selection.stroke
        } else {
            visuals.bg_stroke
        },
        egui::StrokeKind::Inside,
    );
    // Only mask glyphs enter layout/accessibility; no plaintext undo history.
    let count = value.chars().count();
    let caret = value[..cursor.offset].chars().count();
    let columns = ((rect.width() - 12.0) / bullet_width).floor().max(1.0) as usize;
    cursor.first_visible = cursor.first_visible.min(count);
    if caret < cursor.first_visible {
        cursor.first_visible = caret;
    }
    if caret >= cursor.first_visible + columns {
        cursor.first_visible = caret.saturating_sub(columns - 1);
    }
    if !response.has_focus() {
        cursor.first_visible = 0;
    }
    let visible = count.saturating_sub(cursor.first_visible).min(columns);
    let painter = ui.painter().with_clip_rect(rect.shrink(3.0));
    if response.has_focus() && cursor.all {
        painter.rect_filled(
            egui::Rect::from_min_size(
                rect.min + egui::vec2(5.0, 3.0),
                egui::vec2(visible as f32 * bullet_width, rect.height() - 6.0),
            ),
            0.0,
            ui.visuals().selection.bg_fill,
        );
    }
    painter.text(
        rect.left_center() + egui::vec2(5.0, 0.0),
        egui::Align2::LEFT_CENTER,
        "•".repeat(visible),
        font,
        visuals.text_color(),
    );
    if response.has_focus() {
        let x = rect.min.x + 5.0 + caret.saturating_sub(cursor.first_visible) as f32 * bullet_width;
        let cursor_rect = egui::Rect::from_min_max(
            egui::pos2(x, rect.min.y + 4.0),
            egui::pos2(x + 1.0, rect.max.y - 4.0),
        );
        if ui.input(|input| (input.time - cursor.last_activity).rem_euclid(1.0) < 0.5) {
            painter.line_segment(
                [cursor_rect.min, egui::pos2(x, cursor_rect.max.y)],
                egui::Stroke::new(1.0, visuals.text_color()),
            );
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
        ui.output_mut(|output| {
            output.ime = Some(egui::output::IMEOutput {
                rect,
                cursor_rect,
                purpose: egui::IMEPurpose::Password,
                should_interrupt_composition: false,
            })
        });
    }
    ui.data_mut(|data| data.insert_temp(id, cursor));
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::TextEdit, ui.is_enabled(), "密码输入")
    });
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn multiple_keystrokes_have_one_mask_each_and_keep_editing_at_the_caret() {
        let context = egui::Context::default();
        let mut value = String::new();
        let mut id = egui::Id::NULL;
        let mut draw = |events| {
            let mut output = context.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    id = edit(ui, &mut value, "typing", 128).id;
                },
            );
            output.textures_delta.clear();
            output
        };
        draw(vec![]);
        drop(draw);
        context.memory_mut(|memory| memory.request_focus(id));
        let mut draw = |events| {
            let mut output = context.run_ui(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ui| {
                    edit(ui, &mut value, "typing", 128);
                },
            );
            output.textures_delta.clear();
            output
        };
        for (index, character) in ["a", "b", "c"].into_iter().enumerate() {
            let output = draw(vec![egui::Event::Text(character.into())]);
            assert!(
                output.shapes.iter().any(|shape| matches!(&shape.shape,
                egui::epaint::Shape::Text(text) if text.galley.job.text == "•".repeat(index + 1))),
                "the mask must visibly track each typed character"
            );
        }
        let key = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        let output = draw(vec![
            key(egui::Key::ArrowLeft, egui::Modifiers::NONE),
            egui::Event::Text("中".into()),
        ]);
        assert!(output.shapes.iter().any(|shape| matches!(&shape.shape, egui::epaint::Shape::Text(text) if text.galley.job.text == "••••")));
        drop(draw);
        assert_eq!(value, "ab中c");
        let mut output = context.run_ui(
            egui::RawInput {
                events: vec![key(egui::Key::Backspace, egui::Modifiers::NONE)],
                ..Default::default()
            },
            |ui| {
                edit(ui, &mut value, "typing", 128);
            },
        );
        output.textures_delta.clear();
        assert_eq!(value, "abc");
        let mut output = context.run_ui(
            egui::RawInput {
                events: vec![
                    key(egui::Key::A, egui::Modifiers::CTRL),
                    egui::Event::Paste("replacement".into()),
                ],
                ..Default::default()
            },
            |ui| {
                edit(ui, &mut value, "typing", 128);
            },
        );
        output.textures_delta.clear();
        assert_eq!(value, "replacement");
        value.zeroize();
    }
    #[test]
    fn password_text_paste_and_ime_never_enter_paint_accessibility_or_input_history() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut value = String::new();
        let mut id = egui::Id::NULL;
        let mut initial = context.run_ui(egui::RawInput::default(), |ui| {
            id = edit(ui, &mut value, "secret", 128).id;
        });
        initial.textures_delta.clear();
        context.memory_mut(|memory| memory.request_focus(id));
        let mut output = context.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::Paste("PRIVATE-".into()),
                    egui::Event::Ime(egui::ImeEvent::Commit("密码".into())),
                ],
                ..Default::default()
            },
            |ui| {
                edit(ui, &mut value, "secret", 128);
            },
        );
        assert_eq!(value, "PRIVATE-密码");
        let rendered = format!(
            "{:?}{:?}",
            output.shapes, output.platform_output.accesskit_update
        );
        assert!(!rendered.contains("PRIVATE"));
        assert!(!rendered.contains("密码\""));
        context.input(|input| {
            for event in input.events.iter().chain(input.raw.events.iter()) {
                match event {
                    egui::Event::Paste(text) | egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                        assert!(text.is_empty())
                    }
                    _ => {}
                }
            }
        });
        assert_eq!(
            output.platform_output.ime.map(|ime| ime.purpose),
            Some(egui::IMEPurpose::Password)
        );
        let mut last = context.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Backspace,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                edit(ui, &mut value, "secret", 128);
            },
        );
        last.textures_delta.clear();
        output.textures_delta.clear();
        assert_eq!(value, "PRIVATE-密");
        value.zeroize();
    }
}
