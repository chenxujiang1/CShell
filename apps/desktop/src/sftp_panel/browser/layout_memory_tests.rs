use super::tests::{draw, fixture};
use super::*;
use crate::ui_layout_gate::{check_window, context};
use egui::{Event, PointerButton, Pos2, Vec2};
fn pointer(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    }
}
fn drag(
    ctx: &egui::Context,
    panel: &mut SftpPanel,
    view: &SftpView,
    size: Vec2,
    from: Pos2,
    to: Pos2,
) {
    draw(
        ctx,
        panel,
        view,
        size,
        vec![Event::PointerMoved(from), pointer(from, true)],
    );
    draw(
        ctx,
        panel,
        view,
        size,
        vec![Event::PointerMoved(from + (to - from) * 0.25)],
    );
    draw(ctx, panel, view, size, vec![Event::PointerMoved(to)]);
    draw(ctx, panel, view, size, vec![pointer(to, false)]);
    for _ in 0..4 {
        draw(ctx, panel, view, size, vec![]);
    }
}
#[test]
fn pane_and_transfer_drag_stop_at_release_and_survive_session_changes() {
    let ctx = context();
    let (mut panel, view, _) = fixture();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..4 {
        draw(&ctx, &mut panel, &view, size, vec![]);
    }
    let split = panel.layout.split_rect.unwrap().center();
    drag(
        &ctx,
        &mut panel,
        &view,
        size,
        split,
        split + egui::vec2(140.0, 0.0),
    );
    assert!(panel.layout.value.left_ratio > 0.58, "split must move");
    let left_width = panel.browser.local_rect.unwrap().width();
    for _ in 0..6 {
        draw(
            &ctx,
            &mut panel,
            &view,
            size,
            vec![Event::PointerMoved(split - egui::vec2(120.0, 0.0))],
        );
    }
    assert!(
        (panel.browser.local_rect.unwrap().width() - left_width).abs() < 0.5,
        "released split must not continue moving"
    );
    let transfer = panel.layout.transfer_rect.unwrap().center();
    let old = panel.layout.value.transfer_ratio;
    drag(
        &ctx,
        &mut panel,
        &view,
        size,
        transfer,
        transfer - egui::vec2(0.0, 65.0),
    );
    assert!(panel.layout.value.transfer_ratio > old + 0.05);
    let saved = panel.layout.value.clone();
    let next = SessionId::new();
    let mut changed = view.clone();
    changed.session_id = Some(next);
    draw(&ctx, &mut panel, &changed, size, vec![]);
    assert_eq!(
        panel.layout.value, saved,
        "session reset retains global layout"
    );
}
#[test]
fn header_boundary_resizes_column_without_sorting_or_affecting_other_pane() {
    let ctx = context();
    let (mut panel, view, _) = fixture();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..4 {
        draw(&ctx, &mut panel, &view, size, vec![]);
    }
    let rect = panel
        .layout
        .column_handles
        .iter()
        .find(|(local, index, rect)| *local && *index == 0 && rect.is_positive())
        .unwrap()
        .2;
    let initial = columns::widths(true, panel.browser.local_rect.unwrap().width() - 18.0)[0];
    let from = rect.center();
    drag(
        &ctx,
        &mut panel,
        &view,
        size,
        from,
        from + egui::vec2(55.0, 0.0),
    );
    assert!((panel.layout.value.columns[0][0] - initial - 55.0).abs() < 3.0);
    assert!(panel.layout.value.columns[1].is_empty());
    assert_eq!(
        panel.browser.sort[0].unwrap(),
        columns::SortState::initial(true)
    );
    let width = panel.layout.value.columns[0][0];
    draw(
        &ctx,
        &mut panel,
        &view,
        size,
        vec![Event::PointerMoved(from + egui::vec2(200.0, 0.0))],
    );
    assert_eq!(panel.layout.value.columns[0][0], width);
}
#[test]
fn extreme_saved_ratios_keep_both_lists_and_queue_accessible_in_small_windows() {
    for left in [0.15, 0.85] {
        for transfer in [0.15, 0.65] {
            let ctx = context();
            let (mut panel, view, _) = fixture();
            panel.layout.value.left_ratio = left;
            panel.layout.value.transfer_ratio = transfer;
            let size = egui::vec2(720.0, 480.0);
            for _ in 0..4 {
                draw(&ctx, &mut panel, &view, size, vec![]);
            }
            let (tree, _) = draw(&ctx, &mut panel, &view, size, vec![]);
            check_window(&ctx, size, "SFTP 文件", &tree);
            let local = panel.browser.local_rect.unwrap();
            let remote = panel.browser.remote_rect.unwrap();
            assert!(
                local.right() < remote.left(),
                "left ratio={left} transfer={transfer}, local={local:?} remote={remote:?}"
            );
            assert!(local.width() >= 258.0 && remote.width() >= 258.0);
            assert!(
                panel.layout.transfer_rect.unwrap().center().y >= local.bottom() - 2.0,
                "transfer={:?} local={local:?} split={:?}",
                panel.layout.transfer_rect,
                panel.layout.split_rect
            );
        }
    }
}
