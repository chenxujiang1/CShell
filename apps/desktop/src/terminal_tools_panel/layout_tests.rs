use super::*;
use crate::ui_layout_gate::{check_window, context, frame};

#[test]
fn all_terminal_tool_pages_fit_with_populated_rule_editors() {
    for size in [egui::vec2(900.0, 600.0), egui::vec2(720.0, 480.0)] {
        let context = context();
        let mut panel = TerminalToolsPanel::default();
        panel.open = true;
        let mut catalog = TerminalToolsCatalog::default();
        catalog.highlights.push(HighlightSet {
            name: "中文高亮名称".repeat(8),
            profiles: Default::default(),
            rules: vec![HighlightRule {
                id: "long-rule".repeat(8),
                pattern: OutputPattern::Literal {
                    text: "ERROR".into(),
                    case_sensitive: false,
                },
                style: Default::default(),
                enabled: true,
            }],
        });
        catalog.triggers.push(TriggerRule {
            id: "long-trigger".repeat(8),
            profiles: Default::default(),
            pattern: OutputPattern::Literal {
                text: "ERROR".into(),
                case_sensitive: false,
            },
            action: TriggerAction::Notify {
                message: "中文通知".repeat(20),
            },
            cooldown_ms: 5000,
            per_minute: 10,
            enabled: true,
        });
        panel.draft = Some(catalog);
        for page in 0..6 {
            panel.page = page;
            for _ in 0..2 {
                frame(&context, size, |c| {
                    assert!(panel.draw(c, None, None, None).is_none());
                });
            }
            let tree = frame(&context, size, |c| {
                assert!(panel.draw(c, None, None, None).is_none());
            });
            check_window(&context, size, "终端工具", &tree);
        }
    }
}
