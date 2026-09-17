use cshell_render::TerminalSelection;
use cshell_terminal::{CellWidth, FrameSnapshot};

const MAX_ACCESSIBLE_ROWS: u16 = 200;
const MAX_ACCESSIBLE_COLS: u16 = 240;
const MAX_ACCESSIBLE_BYTES: usize = 32 * 1024;

struct VisibleText {
    value: String,
    truncated: bool,
}

pub fn terminal_node_id() -> egui::Id {
    egui::Id::new("cshell-terminal-surface")
}

/// Publish only the visible screen, and only when a platform accessibility
/// client has activated egui's AccessKit tree for this frame.
pub fn add_terminal_node(
    context: &egui::Context,
    rect: egui::Rect,
    snapshot: Option<&FrameSnapshot>,
    selection: Option<TerminalSelection>,
) {
    if !rect.is_positive() {
        return;
    }
    context.accesskit_node_builder(terminal_node_id(), |node| {
        node.set_role(egui::accesskit::Role::Terminal);
        node.set_label("终端");
        node.add_action(egui::accesskit::Action::Focus);
        node.set_bounds(egui::accesskit::Rect {
            x0: f64::from(rect.min.x),
            y0: f64::from(rect.min.y),
            x1: f64::from(rect.max.x),
            y1: f64::from(rect.max.y),
        });
        if let Some(snapshot) = snapshot {
            let visible = visible_text(snapshot);
            node.set_value(visible.value);
            let mut description = format!(
                "光标：第 {} 行，第 {} 列",
                snapshot.cursor_row.saturating_add(1),
                snapshot.cursor_col.saturating_add(1),
            );
            if let Some(selection) = selection {
                description.push_str(&format!(
                    "；已选中：第 {} 行第 {} 列至第 {} 行第 {} 列",
                    selection.anchor.row.saturating_add(1),
                    selection.anchor.column.saturating_add(1),
                    selection.focus.row.saturating_add(1),
                    selection.focus.column.saturating_add(1),
                ));
            }
            if visible.truncated {
                description.push_str("；可访问内容已截断为当前屏幕的一部分");
            }
            node.set_description(description);
        } else {
            node.set_description("尚无终端内容");
        }
    });
}

fn visible_text(snapshot: &FrameSnapshot) -> VisibleText {
    let mut value = String::new();
    let rows = snapshot.rows.min(MAX_ACCESSIBLE_ROWS);
    let cols = snapshot.cols.min(MAX_ACCESSIBLE_COLS);
    let mut truncated = rows < snapshot.rows || cols < snapshot.cols;
    'rows: for row in 0..rows {
        if row > 0 {
            if value.len() == MAX_ACCESSIBLE_BYTES {
                truncated = true;
                break;
            }
            value.push('\n');
        }
        let Some(cells) = snapshot.row(row) else {
            truncated = true;
            break;
        };
        let mut last_non_space = value.len();
        for cell in cells.iter().take(usize::from(cols)) {
            if matches!(
                cell.width,
                CellWidth::WideSpacer | CellWidth::LeadingWideSpacer
            ) {
                continue;
            }
            for character in cell.characters() {
                if value.len() + character.len_utf8() > MAX_ACCESSIBLE_BYTES {
                    value.truncate(last_non_space);
                    truncated = true;
                    break 'rows;
                }
                value.push(character);
                if character != ' ' {
                    last_non_space = value.len();
                }
            }
        }
        value.truncate(last_non_space);
    }
    VisibleText { value, truncated }
}

#[cfg(test)]
mod tests {
    use super::{MAX_ACCESSIBLE_BYTES, add_terminal_node, visible_text};
    use cshell_domain::TerminalSize;
    use cshell_terminal::{Cell, CellWidth, ProbeTerminalEngine, Style, TerminalEngine};

    #[test]
    fn visible_text_preserves_graphemes_without_wide_spacers_or_padding() {
        let mut snapshot = ProbeTerminalEngine::new(TerminalSize::cells(2, 5)).snapshot();
        snapshot.cells[0] =
            Cell::with_zerowidth('e', ['\u{301}'], CellWidth::Single, Style::default());
        snapshot.cells[1] = Cell::new('界', CellWidth::Wide, Style::default());
        snapshot.cells[2] = Cell::new(' ', CellWidth::WideSpacer, Style::default());
        snapshot.cells[5] = Cell::new('B', CellWidth::Single, Style::default());
        let text = visible_text(&snapshot);
        assert_eq!(text.value, "e\u{301}界\nB");
        assert!(!text.truncated);
    }

    #[test]
    fn visible_text_caps_large_snapshots_at_utf8_boundaries() {
        let mut snapshot = ProbeTerminalEngine::new(TerminalSize::cells(200, 240)).snapshot();
        snapshot.cells.fill(Cell::with_zerowidth(
            '界',
            ['\u{301}'],
            CellWidth::Single,
            Style::default(),
        ));
        let text = visible_text(&snapshot);
        assert!(text.truncated);
        assert!(text.value.len() <= MAX_ACCESSIBLE_BYTES);
        assert!(text.value.is_char_boundary(text.value.len()));
    }

    #[test]
    fn visible_text_ignores_missing_or_hidden_rows() {
        let mut snapshot = ProbeTerminalEngine::new(TerminalSize::cells(3, 2)).snapshot();
        snapshot.cells[0] = Cell::new('A', CellWidth::Single, Style::default());
        snapshot.cells.truncate(2);
        let text = visible_text(&snapshot);
        assert_eq!(text.value, "A\n");
        assert!(text.truncated);
    }

    #[test]
    fn terminal_node_is_emitted_only_after_accessibility_activation() {
        let context = egui::Context::default();
        let snapshot = ProbeTerminalEngine::new(TerminalSize::cells(1, 2)).snapshot();
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(80.0, 20.0));
        let mut inactive = context.run_ui(egui::RawInput::default(), |ui| {
            add_terminal_node(ui.ctx(), rect, Some(&snapshot), None);
        });
        assert!(inactive.platform_output.accesskit_update.is_none());
        inactive.textures_delta.clear();

        context.enable_accesskit();
        let mut active = context.run_ui(egui::RawInput::default(), |ui| {
            add_terminal_node(ui.ctx(), rect, Some(&snapshot), None);
        });
        active.textures_delta.clear();
        let Some(update) = active.platform_output.accesskit_update else {
            panic!("activated accessibility should emit a tree update");
        };
        let Some((_, terminal)) = update
            .nodes
            .iter()
            .find(|(_, node)| node.role() == egui::accesskit::Role::Terminal)
        else {
            panic!("terminal node should be in the active accessibility tree");
        };
        assert_eq!(terminal.label(), Some("终端"));
        assert_eq!(terminal.value(), Some(""));
        assert!(terminal.supports_action(egui::accesskit::Action::Focus));
        assert!(terminal.bounds().is_some());
    }
}
