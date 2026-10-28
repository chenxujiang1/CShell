use super::*;
use crate::ui_layout_gate::{bounds, check_window, context};
use cshell_application::{
    SftpQueueItem, SftpQueueSnapshot, SftpVerifiedContent, SftpVerifiedTransfer,
};
use egui::{Event, PointerButton, Pos2, Vec2, accesskit};
mod preview;
#[test]
fn session_verification_override_is_frozen_into_each_new_transfer() {
    let (mut panel,mut view,session)=fixture();let profile=cshell_domain::ProfileId::new();view.connection_profile=Some(profile);
    let file=DragFile{session,local:true,name:"policy.bin".into(),path:if cfg!(windows){"C:/policy.bin"}else{"/policy.bin"}.into()};
    panel.verification_overrides.insert(profile,cshell_domain::SftpVerification::Full);
    panel.stage_file(session,&view,&file,false);
    assert_eq!(panel.browser.staged[0].verification,Some(cshell_domain::SftpVerification::Full));
    panel.verification_overrides.insert(profile,cshell_domain::SftpVerification::Standard);
    assert_eq!(panel.browser.staged[0].verification,Some(cshell_domain::SftpVerification::Full));
    panel.browser.staged.clear();panel.stage_file(session,&view,&file,false);
    assert_eq!(panel.browser.staged[0].verification,Some(cshell_domain::SftpVerification::Standard));
    panel.browser.staged.clear();panel.verification_overrides.clear();panel.stage_file(session,&view,&file,false);
    assert_eq!(panel.browser.staged[0].verification,Some(crate::zmodem_settings::load().sftp_verification));
}

#[test]
fn horizontal_scrolling_reveals_owner_header_and_keeps_it_clickable() {
    let context = context();
    let (mut panel, view, _) = fixture();
    let size = egui::vec2(900.0, 600.0);
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    let pane = panel.browser.remote_rect.unwrap();
    draw(
        &context,
        &mut panel,
        &view,
        size,
        vec![
            Event::PointerMoved(pane.center()),
            Event::MouseWheel {
                phase: egui::TouchPhase::Move,
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(-900.0, 0.0),
                modifiers: Default::default(),
            },
        ],
    );
    for _ in 0..30 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    let header = tree
        .nodes
        .iter()
        .find(|(_, node)| node.role() == accesskit::Role::Button && node.label() == Some("所有者"))
        .unwrap();
    assert!(
        bounds(&header.1).width() > 20.0,
        "owner column must be visible after horizontal scrolling"
    );
    assert!(pane.contains(button(&tree, "所有者")));
    assert!(click(&context, &mut panel, &view, size, "所有者").is_none());
    assert_eq!(
        panel.browser.sort[1].unwrap().column,
        columns::Column::Owner
    );
    assert!(panel.browser.sort[1].unwrap().ascending);
    assert!(click(&context, &mut panel, &view, size, "所有者 ▲").is_none());
    assert!(!panel.browser.sort[1].unwrap().ascending);
    assert_eq!(
        panel.browser.sort[0].unwrap(),
        columns::SortState::initial(true)
    );
}

#[test]
fn table_headers_toggle_sort_per_pane_and_keep_navigation_rows_pinned() {
    let context = context();
    let (mut panel, mut view, _) = fixture();
    let size = egui::vec2(1280.0, 800.0);
    let entry = |name: &str, kind, size, modified| cshell_ipc::SftpDirectoryEntry {
        name: name.into(),
        path: format!("{}/{name}", local_root()),
        kind,
        size: Some(size),
        modified_unix_seconds: Some(modified),
        ..Default::default()
    };
    view.local_entries = vec![
        entry("old.txt", 1, 10, 100),
        entry("new.txt", 1, 2, 200),
        entry("folder", 2, 0, 50),
    ];
    view.entries = view
        .local_entries
        .iter()
        .cloned()
        .map(|mut entry| {
            entry.path = format!("/home/operator/{}", entry.name);
            entry
        })
        .collect();
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    let positions = |tree: &accesskit::TreeUpdate, name: &str| {
        let mut nodes: Vec<_> = tree
            .nodes
            .iter()
            .filter(|(_, node)| {
                node.role() == accesskit::Role::Button && node.label() == Some(name)
            })
            .map(|(_, node)| bounds(node))
            .collect();
        nodes.sort_by(|a, b| a.min.x.total_cmp(&b.min.x));
        nodes.iter().map(|rect| rect.center().y).collect::<Vec<_>>()
    };
    assert!(positions(&tree, "new.txt")[0] < positions(&tree, "old.txt")[0]);
    assert!(positions(&tree, "old.txt")[0] < positions(&tree, "folder")[0]);
    assert!(positions(&tree, "folder")[1] < positions(&tree, "new.txt")[1]);
    assert!(positions(&tree, "..")[0] < positions(&tree, "new.txt")[0]);
    assert!(click(&context, &mut panel, &view, size, "大小").is_none());
    assert_eq!(panel.browser.sort[0].unwrap().column, columns::Column::Size);
    assert!(panel.browser.sort[0].unwrap().ascending);
    assert_eq!(
        panel.browser.sort[1].unwrap(),
        columns::SortState::initial(false)
    );
    assert!(click(&context, &mut panel, &view, size, "大小 ▲").is_none());
    let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
    assert!(!panel.browser.sort[0].unwrap().ascending);
    assert!(positions(&tree, "old.txt")[0] < positions(&tree, "new.txt")[0]);
    assert!(click(&context, &mut panel, &view, size, "刷新").is_some());
    assert!(!panel.browser.sort[0].unwrap().ascending);
}

#[test]
fn default_folder_menu_can_select_current_folder_or_user_home() {
    let context = context();
    let (mut panel, view, _) = fixture();
    let size = egui::vec2(900.0, 600.0);
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    assert!(click(&context, &mut panel, &view, size, "操作").is_none());
    assert!(click(&context, &mut panel, &view, size, "默认目录").is_none());
    assert!(click(&context, &mut panel, &view, size, "使用当前目录").is_none());
    assert_eq!(panel.default_local_directory, local_root());
    assert!(click(&context, &mut panel, &view, size, "恢复用户目录").is_none());
    assert!(panel.default_local_directory.is_empty());
}

#[test]
fn directory_navigation_uses_loaded_paths_and_includes_ancestors_history_and_children() {
    let context = context();
    let (mut panel, view, session) = fixture();
    let size = egui::vec2(900.0, 600.0);
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    panel.local_directory = "not-yet-navigated".into();
    assert!(
        matches!(click(&context, &mut panel, &view, size, "刷新"), Some(SftpClientCommand::BrowseManaged(id, options)) if id == session && options.local && options.path == local_root())
    );
    for name in [".", ".."] {
        let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
        assert_eq!(
            tree.nodes
                .iter()
                .filter(|(_, n)| n.role() == accesskit::Role::Button && n.label() == Some(name))
                .count(),
            2
        );
        let first = click(&context, &mut panel, &view, size, name);
        let second = click(&context, &mut panel, &view, size, name);
        let describe = |command: &Option<SftpClientCommand>| match command {
            Some(SftpClientCommand::BrowseManaged(_, options)) => {
                format!("local={}, path={}", options.local, options.path)
            }
            other => format!("{other:?}"),
        };
        let details = format!(
            "{name}: first {}, second {}",
            describe(&first),
            describe(&second)
        );
        assert!(
            matches!(second, Some(SftpClientCommand::BrowseManaged(id, options)) if id == session && options.path == if name == "." {local_root().into()} else {parent_path(true,local_root())}),
            "{details}"
        );
    }
    let choices = directory_choices(
        false,
        "/home/operator",
        &["/var/log".into()],
        &[cshell_ipc::SftpDirectoryEntry {
            path: "/home/operator/releases".into(),
            kind: 2,
            ..Default::default()
        }],
    );
    assert_eq!(
        choices,
        [
            "/home/operator",
            "/home",
            "/",
            "/var/log",
            "/home/operator/releases"
        ]
    );
    assert_eq!(parent_path(false, "/"), "/");
}

fn local_root() -> &'static str {
    if cfg!(windows) {
        "C:/Users/operator"
    } else {
        "/home/operator"
    }
}

pub(super) fn fixture() -> (SftpPanel, SftpView, SessionId) {
    let session = SessionId::new();
    let mut panel = SftpPanel::default();
    panel.open = true;
    panel.bound_session_id = Some(session);
    let view = SftpView {
        session_id: Some(session),
        queue_writable: true,
        queue: Some(std::sync::Arc::new(SftpQueueSnapshot::initial(session))),
        local_directory: Some(local_root().into()),
        remote_loaded_directory: Some("/home/operator".into()),
        local_entries: vec![cshell_ipc::SftpDirectoryEntry {
            name: "upload.txt".into(),
            path: format!("{}/upload.txt", local_root()),
            kind: 1,
            size: Some(1024),
            ..Default::default()
        }],
        entries: vec![cshell_ipc::SftpDirectoryEntry {
            name: "download.txt".into(),
            path: "/home/operator/download.txt".into(),
            kind: 1,
            size: Some(2048),
            ..Default::default()
        }],
        ..Default::default()
    };
    (panel, view, session)
}

pub(super) fn draw(
    context: &egui::Context,
    panel: &mut SftpPanel,
    view: &SftpView,
    size: Vec2,
    events: Vec<Event>,
) -> (accesskit::TreeUpdate, Option<SftpClientCommand>) {
    let mut command = None;
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, size)),
            events,
            ..Default::default()
        },
        |ui| command = panel.draw(ui.ctx(), view.session_id, Some(view)),
    );
    output.textures_delta.clear();
    (output.platform_output.accesskit_update.unwrap(), command)
}

fn button(tree: &accesskit::TreeUpdate, name: &str) -> Pos2 {
    bounds(
        &tree
            .nodes
            .iter()
            .filter(|(_, n)| n.role() == accesskit::Role::Button && n.label() == Some(name))
            .min_by(|(_, a), (_, b)| bounds(a).min.x.total_cmp(&bounds(b).min.x))
            .unwrap_or_else(|| panic!("missing button {name}"))
            .1,
    )
    .center()
}

fn pointer(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    }
}

pub(super) fn click(
    context: &egui::Context,
    panel: &mut SftpPanel,
    view: &SftpView,
    size: Vec2,
    name: &str,
) -> Option<SftpClientCommand> {
    let (tree, _) = draw(context, panel, view, size, vec![]);
    let pos = button(&tree, name);
    draw(
        context,
        panel,
        view,
        size,
        vec![Event::PointerMoved(pos), pointer(pos, true)],
    );
    draw(context, panel, view, size, vec![pointer(pos, false)]).1
}

#[test]
fn dragging_between_lists_submits_immediately_and_accepts_more_files_while_busy() {
    let context = context();
    let (mut panel, view, session) = fixture();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..3 {
        draw(&context, &mut panel, &view, size, vec![]);
    }
    for (name, local_target) in [("upload.txt", false), ("download.txt", true)] {
        let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
        let from = button(&tree, name);
        let target = if local_target {
            panel.browser.local_rect.unwrap()
        } else {
            panel.browser.remote_rect.unwrap()
        };
        let to = target.center_bottom() - egui::vec2(0.0, 30.0);
        draw(
            &context,
            &mut panel,
            &view,
            size,
            vec![Event::PointerMoved(from), pointer(from, true)],
        );
        draw(
            &context,
            &mut panel,
            &view,
            size,
            vec![Event::PointerMoved(from + egui::vec2(15.0, 15.0))],
        );
        draw(
            &context,
            &mut panel,
            &view,
            size,
            vec![Event::PointerMoved(to)],
        );
        let command = draw(&context, &mut panel, &view, size, vec![pointer(to, false)]).1;
        if local_target {
            assert!(
                command.is_none(),
                "second drop waits for first acknowledgment"
            );
            assert_eq!(panel.browser.staged.len(), 1);
            assert_eq!(
                panel.browser.staged[0].direction,
                SftpQueueDirection::Download
            );
        } else {
            assert!(
                matches!(command, Some(SftpClientCommand::TransferBatch(id, drafts, 1, true)) if id == session && drafts.len() == 1 && drafts[0].remote_path == "/home/operator/upload.txt")
            );
            assert_eq!(panel.browser.submitted_drafts.len(), 1);
        }
    }
    assert!(
        draw(&context, &mut panel, &view, size, vec![]).1.is_none(),
        "no duplicate submission"
    );
    panel.request_rejected();
    assert_eq!(panel.browser.staged.len(), 2);
}

#[test]
fn transfer_controls_remain_visible_below_both_lists_in_small_windows() {
    for size in [
        egui::vec2(1280.0, 800.0),
        egui::vec2(900.0, 600.0),
        egui::vec2(720.0, 480.0),
    ] {
        let context = context();
        let (mut panel, mut view, session) = fixture();
        let item = SftpQueueItem {
            id: SftpQueueItemId::new(),
            state: SftpQueueState::Running,
            draft: SftpQueueDraft {
                verified: true,
                local_path: "C:/Users/operator/upload.txt".into(),
                remote_path: "/home/operator/upload.txt".into(),
                ..Default::default()
            },
            transfer_id: Some(SessionId::new()),
            receipt: None,
            verified: Some(SftpVerifiedTransfer {
                temporary_owned: true,
                bytes: 256,
                content: Some(SftpVerifiedContent {
                    size: 1024,
                    sha256: [1; 32],
                }),
                ..Default::default()
            }),
        };
        let item_id = item.id;
        std::sync::Arc::make_mut(view.queue.as_mut().unwrap())
            .items
            .push(item);
        for _ in 0..3 {
            draw(&context, &mut panel, &view, size, vec![]);
        }
        let (tree, _) = draw(&context, &mut panel, &view, size, vec![]);
        check_window(&context, size, "SFTP 文件", &tree);
        let local = panel.browser.local_rect.unwrap();
        let remote = panel.browser.remote_rect.unwrap();
        assert!(local.right() <= remote.left());
        for name in ["暂停", "取消"] {
            let point = button(&tree, name);
            assert!(
                point.y > local.bottom().max(remote.bottom()),
                "{name} must be in bottom dock"
            );
            assert!(egui::Rect::from_min_size(Pos2::ZERO, size).contains(point));
        }
        assert!(
            matches!(click(&context, &mut panel, &view, size, "暂停"), Some(SftpClientCommand::StopQueueItem(id, item, 1, true)) if id == session && item == item_id)
        );
        assert!(
            matches!(click(&context, &mut panel, &view, size, "取消"), Some(SftpClientCommand::StopQueueItem(id, item, 1, false)) if id == session && item == item_id)
        );
        view.stopping_items.insert(item_id);
        assert!(click(&context, &mut panel, &view, size, "暂停").is_none());
        assert!(click(&context, &mut panel, &view, size, "取消").is_none());
        view.stopping_items.clear();
        let queue = std::sync::Arc::make_mut(view.queue.as_mut().unwrap());
        queue.items[0].verified = Some(SftpVerifiedTransfer::default());
        let transfer = queue.items[0].transfer_id.unwrap();
        view.paused_items.insert(item_id);
        view.queue_transfers.insert(
            transfer,
            cshell_ipc::SftpTransfer {
                transfer_id: transfer.as_uuid().as_bytes().to_vec(),
                state: SftpTransferState::Running as i32,
                paused: true,
                ..Default::default()
            },
        );
        assert!(
            matches!(click(&context, &mut panel, &view, size, "继续"), Some(SftpClientCommand::ResumeQueueItem(id, item, 1)) if id == session && item == item_id)
        );
    }
}

#[test]
fn staging_deduplicates_and_rejects_cross_session_or_escaping_names() {
    let (mut panel, view, session) = fixture();
    let mut file = DragFile {
        session,
        local: true,
        path: format!("{}/file.txt", local_root()),
        name: "file.txt".into(),
    };
    panel.stage_file(session, &view, &file, false);
    panel.stage_file(session, &view, &file, false);
    assert_eq!(panel.browser.staged.len(), 1);
    file.name = "../escape".into();
    panel.stage_file(session, &view, &file, false);
    file.name = "other.txt".into();
    file.session = SessionId::new();
    panel.stage_file(session, &view, &file, false);
    assert_eq!(panel.browser.staged.len(), 1);
    panel.browser.submitted_drafts = std::mem::take(&mut panel.browser.staged);
    panel.browser.auto_submit = false;
    panel.browser.submitted_revision = Some(1);
    let mut uncertain = view.clone();
    uncertain.queue_writable = false;
    uncertain.error = Some("start result uncertain".into());
    draw(
        &context(),
        &mut panel,
        &uncertain,
        egui::vec2(1280.0, 800.0),
        vec![],
    );
    assert_eq!(panel.browser.submitted_revision, Some(1));
    assert_eq!(panel.browser.submitted_drafts.len(), 1);
    let mut changed = view.clone();
    let mut queue = (**view.queue.as_ref().unwrap()).clone();
    queue.revision += 1;
    queue.items.push(SftpQueueItem {
        id: SftpQueueItemId::new(),
        draft: panel.browser.submitted_drafts[0].clone(),
        state: SftpQueueState::Pending,
        transfer_id: None,
        verified: None,
        receipt: None,
    });
    changed.queue = Some(std::sync::Arc::new(queue));
    draw(
        &context(),
        &mut panel,
        &changed,
        egui::vec2(1280.0, 800.0),
        vec![],
    );
    assert!(panel.browser.staged.is_empty());
    let next = SessionId::new();
    draw(
        &context(),
        &mut panel,
        &SftpView {
            session_id: Some(next),
            ..Default::default()
        },
        egui::vec2(1280.0, 800.0),
        vec![],
    );
    assert!(panel.browser.staged.is_empty());
}
