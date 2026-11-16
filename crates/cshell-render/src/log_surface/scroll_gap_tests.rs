use super::*;

fn page(revision: u64, ids: Range<u64>, total: u64) -> Arc<LogPage> {
    let first = ids.start;
    let after = ids.end - 1 < total;
    Arc::new(LogPage {
        first_line_id: 1,
        source_id: LogSourceId(0),
        revision,
        anchor_line_id: first,
        rows: ids
            .map(|line_id| LogRow {
                line_id,
                text: format!("row {line_id}").into(),
                soft_wrap: false,
                style_spans: Default::default(),
                truncated: false,
            })
            .collect::<Vec<_>>()
            .into(),
        total_line_count: total,
        has_before: first > 1,
        has_after: after,
    })
}

fn reflow(surface: &mut LogSurfaceModel, columns: u16) {
    let layout = surface.request_reflow(columns).unwrap().execute().unwrap();
    assert!(surface.submit_reflow(layout));
}

fn ready_surface() -> LogSurfaceModel {
    let mut surface = LogSurfaceModel::default();
    surface.submit_page(page(1, 100..110, 200)).unwrap();
    reflow(&mut surface, 80);
    surface.anchor(104, 0);
    surface
}

fn assert_filled(surface: &LogSurfaceModel, rows: u16) {
    let frame = surface.prepare_frame(rows).unwrap();
    let start = frame.visible_rows.start + frame.first_viewport_row;
    assert!(
        frame.visual_rows.len().saturating_sub(start) >= usize::from(rows),
        "downward scroll left an artificial blank at the bottom"
    );
}

#[test]
fn downward_scroll_waiting_for_the_next_page_keeps_a_full_viewport() {
    let mut surface = ready_surface();
    surface.scroll_visual_rows(3, 5);
    assert_filled(&surface, 5);
    assert_eq!(
        surface.mode(),
        ScrollMode::Anchored {
            line_id: 105,
            cell_offset: 0
        }
    );
    assert!(!surface.take_page_request(5).unwrap().follow_tail);
    surface.submit_page(page(2, 100..130, 200)).unwrap();
    assert_filled(&surface, 5);
    reflow(&mut surface, 80);
    assert_filled(&surface, 5);
    assert_eq!(
        surface.mode(),
        ScrollMode::Anchored {
            line_id: 107,
            cell_offset: 0
        }
    );
}

#[test]
fn repeated_wheel_events_are_accumulated_while_the_page_is_in_flight() {
    let mut surface = ready_surface();
    surface.scroll_visual_rows(4, 5);
    surface.scroll_visual_rows(2, 5);
    assert_filled(&surface, 5);
    surface.submit_page(page(2, 100..150, 200)).unwrap();
    reflow(&mut surface, 80);
    assert_eq!(
        surface.mode(),
        ScrollMode::Anchored {
            line_id: 110,
            cell_offset: 0
        }
    );
}

#[test]
fn reversing_wheel_direction_cancels_the_pending_distance() {
    let mut surface = ready_surface();
    surface.scroll_visual_rows(4, 5);
    surface.scroll_visual_rows(-5, 5);
    assert_filled(&surface, 5);
    assert_eq!(
        surface.mode(),
        ScrollMode::Anchored {
            line_id: 103,
            cell_offset: 0
        }
    );
    surface.submit_page(page(2, 100..150, 200)).unwrap();
    reflow(&mut surface, 80);
    assert_eq!(
        surface.mode(),
        ScrollMode::Anchored {
            line_id: 103,
            cell_offset: 0
        }
    );
}

#[test]
fn upward_wheel_events_also_accumulate_without_losing_the_original_destination() {
    let mut surface = ready_surface();
    surface.anchor(100, 0);
    surface.scroll_visual_rows(-3, 5);
    surface.scroll_visual_rows(-2, 5);
    surface.submit_page(page(2, 90..120, 200)).unwrap();
    reflow(&mut surface, 80);
    assert_eq!(
        surface.mode(),
        ScrollMode::Anchored {
            line_id: 95,
            cell_offset: 0
        }
    );
}

#[test]
fn growing_the_viewport_while_paging_keeps_content_filled_and_preserves_the_anchor() {
    let mut surface = ready_surface();
    assert_filled(&surface, 8);
    assert_eq!(
        surface.mode(),
        ScrollMode::Anchored {
            line_id: 104,
            cell_offset: 0
        }
    );
    surface.submit_page(page(2, 100..130, 200)).unwrap();
    reflow(&mut surface, 80);
    let frame = surface.prepare_frame(8).unwrap();
    let top = frame.visible_rows.start + frame.first_viewport_row;
    assert_eq!(
        frame.page.rows[frame.visual_rows[top].logical_row_index as usize].line_id,
        104
    );
    assert_filled(&surface, 8);
}

#[test]
fn prefetch_starts_before_the_bottom_of_the_viewport_leaves_the_page() {
    let mut surface = LogSurfaceModel::default();
    surface.submit_page(page(1, 100..200, 500)).unwrap();
    reflow(&mut surface, 80);
    surface.take_page_request(20);
    surface.anchor(150, 0);
    surface.take_page_request(20);
    surface.anchor(178, 0);
    assert!(
        surface.take_page_request(20).is_some(),
        "prefetch must cover the entire visible viewport"
    );
}

#[test]
fn next_page_covers_the_pending_scroll_distance_with_bounded_requests() {
    let mut surface = ready_surface();
    surface.scroll_visual_rows(12, 5);
    let request = surface.page_request(5).unwrap();
    assert!(
        request.rows_after >= 5 + 11,
        "request must include the queued destination and a full viewport"
    );
    surface.scroll_visual_rows(i32::MAX, 5);
    let request = surface.page_request(5).unwrap();
    assert!(usize::from(request.rows_before) + usize::from(request.rows_after) < MAX_LOG_PAGE_ROWS);
}

#[test]
fn wrapped_visual_rows_wait_at_the_last_full_screen_instead_of_the_last_line() {
    let wrapped = |revision, ids: Range<u64>| {
        let mut page = (*page(revision, ids, 10)).clone();
        page.rows = page
            .rows
            .iter()
            .map(|row| LogRow {
                text: "abcdefghijklmnopqrstuvwx".into(),
                ..row.clone()
            })
            .collect::<Vec<_>>()
            .into();
        Arc::new(page)
    };
    let mut surface = LogSurfaceModel::default();
    surface.submit_page(wrapped(1, 1..3)).unwrap();
    reflow(&mut surface, 4);
    surface.anchor(1, 16);
    surface.scroll_visual_rows(7, 4);
    assert_filled(&surface, 4);
    surface.submit_page(wrapped(2, 1..5)).unwrap();
    reflow(&mut surface, 4);
    assert_filled(&surface, 4);
    assert_eq!(
        surface.mode(),
        ScrollMode::Anchored {
            line_id: 2,
            cell_offset: 20
        }
    );
}
