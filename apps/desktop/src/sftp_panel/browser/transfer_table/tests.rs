use super::super::tests::{click, draw, fixture};
use super::*;
use crate::ui_layout_gate::{bounds, check_window, context};
use cshell_application::{SftpQueueItem, SftpVerifiedContent, SftpVerifiedTransfer};
fn row(state: SftpQueueState, name: &str) -> SftpQueueItem {
    SftpQueueItem {
        id: SftpQueueItemId::new(),
        draft: SftpQueueDraft {
            verified: true,
            local_path: if cfg!(windows) {
                format!("C:/{name}")
            } else {
                format!("/{name}")
            },
            remote_path: format!("/{name}"),
            ..Default::default()
        },
        state,
        transfer_id: Some(SessionId::new()),
        receipt: None,
        verified: Some(SftpVerifiedTransfer {
            content: Some(SftpVerifiedContent {
                size: 4096,
                sha256: [1; 32],
            }),
            bytes: 1024,
            temporary_owned: true,
            prefix_sha256: Some([2; 32]),
            ..Default::default()
        }),
    }
}
#[test]
fn completed_rows_distinguish_transfer_from_full_verification() {
    let view=SftpView::default();let mut item=row(SftpQueueState::Succeeded,"done.bin");
    assert_eq!(status(&item,&view),"已校验");
    item.draft.verification=Some(cshell_domain::SftpVerification::Standard);
    assert_eq!(status(&item,&view),"已传输");
    item.draft.verification=Some(cshell_domain::SftpVerification::Full);
    assert_eq!(status(&item,&view),"已校验");
}
#[test]
fn filters_and_batch_selection_do_not_include_hidden_tasks() {
    let context = context();
    let (mut panel, mut view, session) = fixture();
    let size = egui::vec2(1280.0, 800.0);
    let active = row(SftpQueueState::Running, "active.bin");
    let failed = row(SftpQueueState::Unknown, "failed.bin");
    std::sync::Arc::make_mut(view.queue.as_mut().unwrap()).items = vec![active, failed.clone()];
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    let pos = bounds(
        &tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::CheckBox
                    && node.label() == Some("全选当前筛选")
            })
            .unwrap()
            .1,
    )
    .center();
    let pointer = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    };
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![egui::Event::PointerMoved(pos), pointer(true)],
    );
    draw(&context, &mut panel, &view, size, vec![pointer(false)]);
    assert_eq!(panel.browser.table.selected.len(), 2);
    panel.browser.table.filter = Filter::Failed;
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    assert_eq!(
        panel
            .browser
            .table
            .selected
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![failed.id]
    );
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, n)| n.label().or(n.value()) == Some("active.bin"))
    );
    click(&context, &mut panel, &view, size, "批量操作");
    let command = click(&context, &mut panel, &view, size, "重试选中 (1)");
    assert!(
        matches!(command,Some(SftpClientCommand::QueueBatch(id,BatchAction::Retry,ids,1))if id==session&&ids==vec![failed.id])
    );
}
#[test]
fn retry_and_manual_review_are_distinct_and_do_not_need_idle_queue() {
    let (_, mut view, _) = fixture();
    let failed = row(SftpQueueState::Unknown, "retry.bin");
    let active = row(SftpQueueState::Running, "active.bin");
    let mut uncertain = failed.clone();
    uncertain.id = SftpQueueItemId::new();
    uncertain.verified.as_mut().unwrap().intent = cshell_application::SftpVerifiedIntent::Create;
    uncertain.verified.as_mut().unwrap().temporary_owned = false;
    assert_eq!(category(&failed, &view), Filter::Failed);
    assert_eq!(category(&uncertain, &view), Filter::Review);
    view.stopping_items.insert(failed.id);
    assert_eq!(
        category(&failed, &view),
        Filter::Cancelled,
        "confirmed cancellation should appear in the cancelled filter"
    );
    uncertain.id = failed.id;
    assert_eq!(
        category(&uncertain, &view),
        Filter::Review,
        "an ambiguous file mutation still needs review after cancel"
    );
    view.stopping_items.clear();
    let queue = std::sync::Arc::make_mut(view.queue.as_mut().unwrap());
    queue.items = vec![failed.clone(), active];
    queue.paused = false;
    assert!(BatchAction::Retry.allows(&failed, &view));
    std::sync::Arc::make_mut(view.queue.as_mut().unwrap())
        .retry_items
        .insert(failed.id);
    assert_eq!(status(&failed, &view), "等待重试");
    assert!(!BatchAction::Retry.allows(&failed, &view));
    assert!(BatchAction::Cancel.allows(&failed, &view));
    std::sync::Arc::make_mut(view.queue.as_mut().unwrap())
        .retry_items
        .clear();
    let mut restarted = failed.clone();
    restarted.state = SftpQueueState::Running;
    restarted.transfer_id = Some(SessionId::new());
    view.paused_items.insert(failed.id);
    let old = failed.transfer_id.unwrap();
    view.queue_transfers.insert(
        old,
        cshell_ipc::SftpTransfer {
            transfer_id: old.as_uuid().as_bytes().to_vec(),
            paused: true,
            ..Default::default()
        },
    );
    assert_eq!(
        category(&restarted, &view),
        Filter::Active,
        "the old transfer's paused marker cannot pause a retry"
    );
    assert!(BatchAction::Pause.allows(&restarted, &view));
    let current = restarted.transfer_id.unwrap();
    view.queue_transfers.insert(
        current,
        cshell_ipc::SftpTransfer {
            transfer_id: current.as_uuid().as_bytes().to_vec(),
            paused: true,
            ..Default::default()
        },
    );
    assert_eq!(category(&restarted, &view), Filter::Paused);
    view.queue_transfers.get_mut(&current).unwrap().paused = false;
    assert_eq!(
        category(&restarted, &view),
        Filter::Active,
        "queue resume must reflect the fresh transfer status"
    );
}
#[test]
fn speed_sampling_resets_on_pause_resume_retry_and_stall() {
    let now = Instant::now();
    let id = Some(SessionId::new());
    let mut sample = Sample {
        transfer: id,
        bytes: 0,
        at: now,
        progress_at: now,
        rate: None,
        active: true,
    };
    sample.update(id, 1024, true, now + std::time::Duration::from_secs(1));
    assert_eq!(sample.rate, Some(1024.0));
    sample.update(id, 1024, false, now + std::time::Duration::from_secs(2));
    assert!(sample.rate.is_none());
    sample.update(id, 1024, true, now + std::time::Duration::from_secs(20));
    assert!(sample.rate.is_none());
    sample.update(id, 2048, true, now + std::time::Duration::from_secs(21));
    assert_eq!(sample.rate, Some(1024.0));
    sample.update(id, 2048, true, now + std::time::Duration::from_secs(25));
    assert!(sample.rate.is_none());
    sample.update(
        Some(SessionId::new()),
        0,
        true,
        now + std::time::Duration::from_secs(26),
    );
    assert!(sample.rate.is_none());
    assert_eq!(remaining(3665), "1时1分");
    assert_eq!(remaining(65), "1分5秒");
}
#[test]
fn compact_table_virtualizes_large_queue_and_fits_small_window() {
    let context = context();
    let (mut panel, mut view, _) = fixture();
    let size = egui::vec2(720.0, 480.0);
    std::sync::Arc::make_mut(view.queue.as_mut().unwrap()).items = (0..128)
        .map(|i| {
            row(
                SftpQueueState::Unknown,
                &format!("long-{}-{i}.bin", "目录".repeat(20)),
            )
        })
        .collect();
    std::sync::Arc::make_mut(view.queue.as_mut().unwrap()).items[127] =
        row(SftpQueueState::Running, "active-last.bin");
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    check_window(&context, size, "SFTP 文件", &tree);
    let checkboxes: Vec<_> = tree
        .nodes
        .iter()
        .filter(|(_, n)| {
            n.role() == egui::accesskit::Role::CheckBox
                && n.label().is_some_and(|s| s.starts_with("选择任务"))
        })
        .collect();
    assert!(!checkboxes.is_empty());
    let first = checkboxes
        .iter()
        .min_by(|(_, a), (_, b)| bounds(a).min.y.total_cmp(&bounds(b).min.y))
        .unwrap();
    assert_eq!(
        first.1.label(),
        Some("选择任务 active-last.bin"),
        "active tasks must be visible ahead of older terminal rows"
    );
    assert!(checkboxes.len() < 12, "virtualize the bounded task table");
    let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
    for (_, node) in checkboxes {
        assert!(viewport.contains_rect(bounds(node)));
    }
}

#[test]
fn unready_drop_shows_queue_recovery_then_submits_once_after_refresh() {
    let ctx = context();
    let (mut panel, mut view, session) = fixture();
    let size = egui::vec2(900.0, 600.0);
    view.queue_writable = false;
    let draft = row(SftpQueueState::Pending, "first.txt").draft;
    panel.browser.staged.push(draft.clone());
    panel.browser.auto_submit = true;
    for _ in 0..3 {
        assert!(draw(&ctx, &mut panel, &view, size, vec![]).1.is_none());
    }
    let (tree, command) = draw(&ctx, &mut panel, &view, size, vec![]);
    assert!(command.is_none());
    assert!(tree.nodes.iter().any(|(_, n)| n.label().or(n.value()) == Some("队列未就绪，请刷新队列")),
        "visible text: {:?}", tree.nodes.iter().map(|(_, n)| (n.role(), n.label(), n.value())).collect::<Vec<_>>());
    assert!(!tree.nodes.iter().any(|(_, n)| n.label().or(n.value()) == Some("等待队列确认")));
    assert!(matches!(click(&ctx, &mut panel, &view, size, "刷新队列"), Some(SftpClientCommand::QueueGet(id)) if id == session));
    view.queue_writable = true;
    view.generation += 1;
    let (_, command) = draw(&ctx, &mut panel, &view, size, vec![]);
    assert!(matches!(command, Some(SftpClientCommand::TransferBatch(id, drafts, 1, true)) if id == session && drafts == vec![draft]));
    assert!(draw(&ctx, &mut panel, &view, size, vec![]).1.is_none(), "never replay a submitted upload");
}

#[test]
fn uncertain_drop_stops_waiting_and_refresh_reconciles_already_added_file() {
    let ctx = context();
    let (mut panel, mut view, session) = fixture();
    let size = egui::vec2(900.0, 600.0);
    let draft = row(SftpQueueState::Pending, "uncertain.txt").draft;
    panel.browser.staged.push(draft.clone());
    panel.browser.auto_submit = true;
    let (_, command) = draw(&ctx, &mut panel, &view, size, vec![]);
    assert!(matches!(command, Some(SftpClientCommand::TransferBatch(..))));
    view.queue_busy = true;
    let (tree, _) = draw(&ctx, &mut panel, &view, size, vec![]);
    assert!(tree.nodes.iter().any(|(_, n)| n.label().or(n.value()) == Some("等待队列确认")),
        "visible text: {:?}", tree.nodes.iter().map(|(_, n)| (n.role(), n.label(), n.value())).collect::<Vec<_>>());
    view.queue_busy = false;
    view.queue_writable = false;
    view.generation += 1;
    view.error = Some("reply timed out".into());
    let (tree, command) = draw(&ctx, &mut panel, &view, size, vec![]);
    assert!(command.is_none());
    assert!(tree.nodes.iter().any(|(_, n)| n.label().or(n.value()) == Some("需核对")));
    assert!(tree.nodes.iter().any(|(_, n)| n.label().or(n.value()) == Some("结果未确认，请刷新队列")));
    assert!(!tree.nodes.iter().any(|(_, n)| n.label().or(n.value()) == Some("等待队列确认")));
    assert!(matches!(click(&ctx, &mut panel, &view, size, "刷新队列"), Some(SftpClientCommand::QueueGet(id)) if id == session));
    let mut confirmed = row(SftpQueueState::Succeeded, "uncertain.txt");
    confirmed.draft = draft;
    let queue = std::sync::Arc::make_mut(view.queue.as_mut().unwrap());
    queue.revision += 1;
    queue.items.push(confirmed);
    view.queue_writable = true;
    view.error = None;
    view.generation += 1;
    assert!(draw(&ctx, &mut panel, &view, size, vec![]).1.is_none());
    assert!(panel.browser.submitted_drafts.is_empty());
    assert!(panel.browser.staged.is_empty(), "already accepted upload must not become a retry draft");
    assert!(draw(&ctx, &mut panel, &view, size, vec![]).1.is_none());
}

#[test]
fn uploaded_file_shows_verification_phase_progress_instead_of_stalled_commit() {
    use cshell_ipc::{SftpTransfer,SftpTransferPhase as Phase};
    let ctx=context();let (mut panel,mut view,_)=fixture();
    let size=egui::vec2(900.0,600.0);
    let mut item=row(SftpQueueState::Running,"large.bin");
    item.verified.as_mut().unwrap().verified=true;
    let transfer=item.transfer_id.unwrap();
    std::sync::Arc::make_mut(view.queue.as_mut().unwrap()).items.push(item.clone());
    view.queue_transfers.insert(transfer,SftpTransfer {
        transfer_id:transfer.as_uuid().as_bytes().to_vec(),bytes_transferred:4096,total_bytes:Some(4096),
        phase:Phase::CheckingPublished as i32,phase_bytes:2048,phase_total:Some(8192),
        ..Default::default()
    });
    for _ in 0..3 {draw(&ctx,&mut panel,&view,size,vec![]);}
    let (tree,_)=draw(&ctx,&mut panel,&view,size,vec![]);
    assert_eq!(status(&item,&view),"核验目标");
    assert!(tree.nodes.iter().any(|(_,n)|n.label().or(n.value()).is_some_and(|s|s.contains("校验 25%"))));
    for (phase,label) in [(Phase::Verifying,"校验文件"),(Phase::Publishing,"发布文件"),(Phase::Cleaning,"清理中"),(Phase::ServerChecksum,"服务器校验")] {
        view.queue_transfers.get_mut(&transfer).unwrap().phase=phase as i32;
        assert_eq!(status(&item,&view),label);
    }
    let (tree,_)=draw(&ctx,&mut panel,&view,size,vec![]);
    assert!(tree.nodes.iter().any(|(_,n)|n.label().or(n.value()).is_some_and(|s|s.contains("服务器计算 SHA-256"))));
}
