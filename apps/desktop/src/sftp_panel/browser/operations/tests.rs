use super::super::tests::{click, draw, fixture};
use super::*;
use crate::ui_layout_gate::{bounds, context};
use egui::{Event, Key, Modifiers, PointerButton, Pos2, Vec2, accesskit};
fn key(key: Key, modifiers: Modifiers) -> Vec<Event> {
    vec![
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        },
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: false,
            repeat: false,
            modifiers,
        },
    ]
}
fn frame(
    ctx: &egui::Context,
    panel: &mut SftpPanel,
    view: &SftpView,
    events: Vec<Event>,
    modifiers: Modifiers,
) -> (accesskit::TreeUpdate, Option<SftpClientCommand>) {
    let mut command = None;
    let mut events = events;
    events.insert(0, Event::ModifiersChanged(modifiers));
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(1280.0, 800.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            command = panel.draw(ui.ctx(), view.session_id, Some(view));
        },
    );
    output.textures_delta.clear();
    (output.platform_output.accesskit_update.unwrap(), command)
}
fn pointer_click(
    ctx: &egui::Context,
    panel: &mut SftpPanel,
    view: &SftpView,
    label: &str,
    button: PointerButton,
    modifiers: Modifiers,
) {
    let (tree, _) = frame(ctx, panel, view, vec![], modifiers);
    let p = bounds(
        &tree
            .nodes
            .iter()
            .find(|(_, n)| n.role() == accesskit::Role::Button && n.label() == Some(label))
            .unwrap()
            .1,
    )
    .center();
    for pressed in [true, false] {
        frame(
            ctx,
            panel,
            view,
            vec![
                Event::PointerMoved(p),
                Event::PointerButton {
                    pos: p,
                    button,
                    pressed,
                    modifiers,
                },
            ],
            modifiers,
        );
    }
}
fn populated() -> (SftpPanel, SftpView, SessionId) {
    let (mut panel, mut view, session) = fixture();
    view.entries = (0..4)
        .map(|i| cshell_ipc::SftpDirectoryEntry {
            name: format!("file-{i}.txt"),
            path: format!("/home/operator/file-{i}.txt"),
            kind: 1,
            permissions: Some(0o100640),
            ..Default::default()
        })
        .collect();
    panel.browser.sort[1] = Some(columns::SortState::initial(false));
    (panel, view, session)
}

#[test]
fn keyboard_selection_follows_sort_and_tab_switches_panes_without_stealing_text(){
    let ctx=context();let(mut panel,view,_)=populated();panel.browser.navigation.active=1;
    for _ in 0..3{frame(&ctx,&mut panel,&view,vec![],Modifiers::NONE);}
    frame(&ctx,&mut panel,&view,key(Key::ArrowDown,Modifiers::NONE),Modifiers::NONE);
    assert_eq!(panel.browser.selection[1].cursor.as_deref(),Some("/home/operator/file-0.txt"));
    frame(&ctx,&mut panel,&view,key(Key::ArrowDown,Modifiers::SHIFT),Modifiers::SHIFT);
    assert_eq!(panel.browser.selection[1].paths.len(),2);
    frame(&ctx,&mut panel,&view,key(Key::End,Modifiers::NONE),Modifiers::NONE);
    assert_eq!(panel.browser.selection[1].cursor.as_deref(),Some("/home/operator/file-3.txt"));assert_eq!(panel.browser.selection[1].paths.len(),1);
    frame(&ctx,&mut panel,&view,key(Key::Tab,Modifiers::NONE),Modifiers::NONE);assert_eq!(panel.browser.navigation.active,0);
}
#[test]
fn ctrl_shift_and_right_click_preserve_sorted_ranges_and_scope() {
    let ctx = context();
    let (mut panel, mut view, _) = populated();
    for _ in 0..3 {
        frame(&ctx, &mut panel, &view, vec![], Modifiers::NONE);
    }
    pointer_click(
        &ctx,
        &mut panel,
        &view,
        "file-0.txt",
        PointerButton::Primary,
        Modifiers::NONE,
    );
    pointer_click(
        &ctx,
        &mut panel,
        &view,
        "file-2.txt",
        PointerButton::Primary,
        Modifiers::CTRL,
    );
    assert_eq!(panel.browser.selection[1].paths.len(), 2);
    pointer_click(
        &ctx,
        &mut panel,
        &view,
        "file-3.txt",
        PointerButton::Primary,
        Modifiers::SHIFT,
    );
    assert_eq!(
        panel.browser.selection[1]
            .paths
            .iter()
            .cloned()
            .collect::<Vec<_>>(),
        vec!["/home/operator/file-2.txt", "/home/operator/file-3.txt"]
    );
    pointer_click(
        &ctx,
        &mut panel,
        &view,
        "file-2.txt",
        PointerButton::Secondary,
        Modifiers::NONE,
    );
    assert_eq!(panel.browser.selection[1].paths.len(), 2);
    let (tree, _) = frame(&ctx, &mut panel, &view, vec![], Modifiers::NONE);
    assert!(
        tree.nodes
            .iter()
            .any(|(_, n)| n.label() == Some("属性 / 权限"))
    );
    // Dismiss the popup before verifying scope changes.
    frame(
        &ctx,
        &mut panel,
        &view,
        key(Key::Escape, Modifiers::NONE),
        Modifiers::NONE,
    );
    view.remote_loaded_directory = Some("/tmp".into());
    view.entries.clear();
    frame(&ctx, &mut panel, &view, vec![], Modifiers::NONE);
    assert!(panel.browser.selection[1].paths.is_empty());
}
#[test]
fn selection_survives_refresh_but_prunes_missing_entries_and_uses_new_sort() {
    let (mut panel, mut view, _) = populated();
    let selection = &mut panel.browser.selection[1];
    selection.sync(view.remote_loaded_directory.as_deref(), &view.entries);
    let order: Vec<_> = view.entries.iter().rev().collect();
    selection.click(&order[0].path, &order, Modifiers::NONE);
    selection.click(&order[2].path, &order, Modifiers::SHIFT);
    assert_eq!(selection.paths.len(), 3);
    selection.sync(None, &[]);
    assert_eq!(selection.paths.len(), 3);
    view.entries.pop();
    selection.sync(view.remote_loaded_directory.as_deref(), &view.entries);
    assert_eq!(selection.paths.len(), 2);
    selection.click(
        &view.entries[0].path,
        &view.entries.iter().collect::<Vec<_>>(),
        Modifiers::CTRL | Modifiers::SHIFT,
    );
    assert!(selection.paths.contains(&view.entries[0].path));
}
#[test]
fn f2_opens_inline_rename_and_does_not_steal_text_edit_shortcuts() {
    let ctx = context();
    let (mut panel, view, _) = populated();
    for _ in 0..3 {
        frame(&ctx, &mut panel, &view, vec![], Modifiers::NONE);
    }
    pointer_click(
        &ctx,
        &mut panel,
        &view,
        "file-1.txt",
        PointerButton::Primary,
        Modifiers::NONE,
    );
    let (_, command) = frame(
        &ctx,
        &mut panel,
        &view,
        key(Key::F2, Modifiers::NONE),
        Modifiers::NONE,
    );
    assert!(command.is_none());
    assert_eq!(panel.page, 0);
    assert_eq!(panel.browser.operation.as_ref().unwrap().name, "file-1.txt");
    frame(
        &ctx,
        &mut panel,
        &view,
        vec![Event::Text("renamed.txt".into())],
        Modifiers::NONE,
    );
    assert_eq!(
        panel.browser.operation.as_ref().unwrap().name,
        "renamed.txt"
    );
    frame(
        &ctx,
        &mut panel,
        &view,
        key(Key::A, Modifiers::CTRL),
        Modifiers::CTRL,
    );
    assert_eq!(panel.browser.selection[1].paths.len(), 1);
    frame(
        &ctx,
        &mut panel,
        &view,
        key(Key::Escape, Modifiers::NONE),
        Modifiers::NONE,
    );
    assert!(panel.browser.operation.is_none());
}

#[test]
fn f4_opens_selected_remote_file_directly_and_never_edits_a_directory(){
    let ctx=context();let (mut panel,view,session)=populated();
    for _ in 0..3 {frame(&ctx,&mut panel,&view,vec![],Modifiers::NONE);}
    pointer_click(&ctx,&mut panel,&view,"file-1.txt",PointerButton::Primary,Modifiers::NONE);
    let (_,command)=frame(&ctx,&mut panel,&view,key(Key::F4,Modifiers::NONE),Modifiers::NONE);
    assert_eq!(panel.page,2);
    let Some(SftpClientCommand::Workspace(id,request))=command else{panic!("editor request missing");};assert_eq!(id,session);assert_eq!(request.operation,SftpOperation::EditOpen);
    let options:cshell_application::SftpBrowseOptions=serde_json::from_slice(&request.data).unwrap();assert_eq!(options.path,"/home/operator/file-1.txt");assert!(!options.local);
    assert!(panel.open_remote_editor(session,&view,"/missing").is_none());
}
#[test]
fn folder_and_file_selection_starts_one_bound_tree_and_keeps_regular_files_on_existing_queue(){
    let (mut panel,mut view,session)=populated();
    let folder=cshell_ipc::SftpDirectoryEntry{name:"folder".into(),path:"/home/operator/folder".into(),kind:2,..Default::default()};view.entries.push(folder.clone());
    panel.browser.selection[1].paths.insert(folder.path.clone());panel.browser.selection[1].paths.insert(view.entries[0].path.clone());
    assert_eq!(panel.drag_selection(session,&view,false,&folder).len(),2);
    panel.transfer_selection(session,&view,false);assert!(panel.browser.staged.is_empty());
    let tree=panel.browser.tree.as_ref().unwrap();assert_eq!(tree.direction,SftpQueueDirection::Download);assert_eq!(tree.paths.len(),2);assert_eq!(Some(tree.destination.as_str()),view.local_directory.as_deref());
    let mut command=None;panel.submit_dropped(session,&view,&mut command);
    assert!(matches!(command,Some(SftpClientCommand::Tree(id,SftpOperation::TreeStart,_,Some(_))) if id==session));
    panel.request_rejected();assert!(panel.browser.tree.is_some());assert!(!panel.browser.tree_auto);
}
#[test]
fn selected_files_transfer_and_drag_as_a_group() {
    let (mut panel, view, session) = populated();
    panel.browser.selection[1].paths = view.entries.iter().map(|e| e.path.clone()).collect();
    let files = panel.drag_selection(session, &view, false, &view.entries[0]);
    assert_eq!(files.len(), 4);
    panel.transfer_selection(session, &view, false);
    assert_eq!(panel.browser.staged.len(), 4);
    panel.transfer_selection(session, &view, false);
    assert_eq!(panel.browser.staged.len(), 4);
    assert!(
        panel
            .browser
            .staged
            .iter()
            .all(|d| d.direction == SftpQueueDirection::Download)
    );
}
fn job(
    _view: &SftpView,
    session: SessionId,
    draft: SftpFileDraft,
    state: SftpFileState,
) -> cshell_application::SftpFileJob {
    cshell_application::SftpFileJob {
        peer: cshell_domain::ClipboardTrust {
            host: cshell_domain::ClipboardHost::new("test.example", 22).unwrap(),
            username: "operator".into(),
            host_key_sha256: "SHA256:test".into(),
            route: String::new(),
        },
        native_transfer: SessionId::new(),
        format: "cshell.sftp-file-job".into(),
        version: 1,
        id: SessionId::new(),
        session,
        revision: 2,
        draft,
        state,
        source: None,
        target: None,
        intent: false,
        acknowledged: false,
        detail: String::new(),
    }
}
fn submit(
    panel: &mut SftpPanel,
    view: &SftpView,
    session: SessionId,
    action: SftpFileAction,
) -> SftpFileDraft {
    panel.browser.selection[1]
        .paths
        .insert(view.entries[0].path.clone());
    panel.open_file_dialog(view, false, action);
    let d = panel.browser.operation.as_mut().unwrap();
    if action == SftpFileAction::Rename {
        d.name = "renamed.txt".into();
    }
    let current = d.drafts().unwrap().remove(0);
    d.run = Some(Run {
        remaining: VecDeque::new(),
        current: current.clone(),
        before: view.file_job.as_ref().map(|j| j.id),
        job: None,
        applied_revision: None,
        generation: view.generation,
        epoch: view.workspace_epoch,
        completed: 0,
        total: 1,
    });
    assert_eq!(panel.bound_session_id, Some(session));
    current
}
#[test]
fn reviewed_operation_applies_once_and_refreshes_after_acknowledged_success() {
    let (mut panel, mut view, session) = populated();
    let draft = submit(&mut panel, &view, session, SftpFileAction::Rename);
    let mut file = job(&view, session, draft, SftpFileState::Inspecting);
    view.file_job = Some(std::sync::Arc::new(file.clone()));
    view.file_writable = true;
    let mut command = None;
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none());
    file.state = SftpFileState::Ready;
    file.revision += 1;
    view.file_job = Some(std::sync::Arc::new(file.clone()));
    panel.advance_file_operation(session, &view, &mut command);
    assert!(
        matches!(command.take(),Some(SftpClientCommand::Workspace(id,c)) if id==session&&c.operation==SftpOperation::FileApply&&c.revision==file.revision&&c.object==file.id.as_uuid().as_bytes())
    );
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none(), "never submit apply twice");
    file.state = SftpFileState::Succeeded;
    file.revision += 2;
    view.file_job = Some(std::sync::Arc::new(file));
    panel.advance_file_operation(session, &view, &mut command);
    assert!(
        matches!(command,Some(SftpClientCommand::BrowseManaged(id,ref opts)) if id==session&&!opts.local&&opts.path=="/home/operator")
    );
    assert!(panel.browser.operation.is_none());
}
#[test]
fn stale_unrelated_jobs_and_uncertain_failures_never_auto_apply() {
    let (mut panel, mut view, session) = populated();
    let draft = submit(&mut panel, &view, session, SftpFileAction::Rename);
    let mut old = job(&view, session, draft.clone(), SftpFileState::Ready);
    panel
        .browser
        .operation
        .as_mut()
        .unwrap()
        .run
        .as_mut()
        .unwrap()
        .before = Some(old.id);
    view.file_job = Some(std::sync::Arc::new(old.clone()));
    view.file_writable = true;
    let mut command = None;
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none());
    old.id = SessionId::new();
    old.session = SessionId::new();
    view.file_job = Some(std::sync::Arc::new(old));
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none());
    view.workspace_epoch += 1;
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none());
    assert!(panel.browser.operation.as_ref().unwrap().error.is_some());
    panel.request_failed(false);
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none());
}
#[test]
fn names_cannot_escape_current_directory_and_permissions_validate() {
    for name in ["", "..", ".", "../oops", "a/b", "a\\b", "x\n"] {
        assert!(child_path(false, "/tmp", name).is_err(), "{name:?}");
    }
    assert_eq!(child_path(false, "/", "你好.txt").unwrap(), "/你好.txt");
    let (mut panel, view, _) = populated();
    panel.browser.selection[1].paths = view.entries.iter().map(|e| e.path.clone()).collect();
    panel.open_file_dialog(&view, false, SftpFileAction::Chmod);
    let d = panel.browser.operation.as_mut().unwrap();
    d.mode = "2750".into();
    let drafts = d.drafts().unwrap();
    assert_eq!(drafts.len(), 4);
    assert!(drafts.iter().all(|d| d.permissions == Some(0o2750)));
    d.mode = "888".into();
    assert!(d.drafts().is_err());
}
#[test]
fn small_window_dialog_exposes_apply_cancel_and_permissions() {
    let ctx = context();
    let (mut panel, view, _) = populated();
    panel.browser.selection[1]
        .paths
        .insert(view.entries[0].path.clone());
    panel.open_file_dialog(&view, false, SftpFileAction::Inspect);
    let size = egui::vec2(720.0, 480.0);
    for _ in 0..4 {
        draw(&ctx, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&ctx, &mut panel, &view, size, vec![]);
    for label in ["应用", "取消"] {
        let rect = bounds(
            &tree
                .nodes
                .iter()
                .find(|(_, n)| n.role() == accesskit::Role::Button && n.label() == Some(label))
                .unwrap()
                .1,
        );
        assert!(
            egui::Rect::from_min_size(Pos2::ZERO, size).contains_rect(rect),
            "{label}: {rect:?}"
        );
    }
    assert!(tree.nodes.iter().any(|(_, n)| n.label() == Some("读取")));
    assert!(click(&ctx, &mut panel, &view, size, "取消").is_none());
    assert!(panel.browser.operation.is_none());
}

#[test]
fn ctrl_shift_n_enter_submits_mkdir_once_and_pending_dialog_cannot_be_dismissed() {
    let ctx = context();
    let (mut panel, view, session) = populated();
    for _ in 0..3 {
        frame(&ctx, &mut panel, &view, vec![], Modifiers::NONE);
    }
    frame(
        &ctx,
        &mut panel,
        &view,
        key(Key::N, Modifiers::CTRL | Modifiers::SHIFT),
        Modifiers::CTRL | Modifiers::SHIFT,
    );
    assert_eq!(
        panel.browser.operation.as_ref().unwrap().action,
        SftpFileAction::Mkdir
    );
    frame(
        &ctx,
        &mut panel,
        &view,
        vec![Event::Text("新目录".into())],
        Modifiers::NONE,
    );
    let (_, command) = frame(
        &ctx,
        &mut panel,
        &view,
        key(Key::Enter, Modifiers::NONE),
        Modifiers::NONE,
    );
    match command {
        Some(SftpClientCommand::Workspace(id, c)) => {
            assert_eq!(id, session);
            assert_eq!(c.operation, SftpOperation::FileInspect);
            let draft: SftpFileDraft = serde_json::from_slice(&c.data).unwrap();
            assert_eq!(draft.action, SftpFileAction::Mkdir);
            assert_eq!(draft.path, "/home/operator/新目录");
            assert_eq!(draft.permissions, Some(0o755));
        }
        _ => panic!("Enter must submit the mkdir draft"),
    }
    assert!(
        frame(
            &ctx,
            &mut panel,
            &view,
            key(Key::Enter, Modifiers::NONE),
            Modifiers::NONE
        )
        .1
        .is_none()
    );
    frame(
        &ctx,
        &mut panel,
        &view,
        key(Key::Escape, Modifiers::NONE),
        Modifiers::NONE,
    );
    assert!(
        panel.browser.operation.is_some(),
        "pending operation keeps its status visible"
    );
}
#[test]
fn batch_permissions_stop_on_first_failure_and_report_completed_count() {
    let (mut panel, mut view, session) = populated();
    panel.browser.selection[1].paths = view.entries.iter().map(|e| e.path.clone()).collect();
    panel.open_file_dialog(&view, false, SftpFileAction::Chmod);
    let dialog = panel.browser.operation.as_mut().unwrap();
    let mut drafts: VecDeque<_> = dialog.drafts().unwrap().into();
    let current = drafts.pop_front().unwrap();
    dialog.run = Some(Run {
        remaining: drafts,
        current: current.clone(),
        before: None,
        job: None,
        applied_revision: Some(2),
        generation: 0,
        epoch: 0,
        completed: 0,
        total: 4,
    });
    let mut file = job(&view, session, current, SftpFileState::Succeeded);
    view.file_job = Some(std::sync::Arc::new(file.clone()));
    view.file_writable = true;
    let mut command = None;
    panel.advance_file_operation(session, &view, &mut command);
    assert!(
        matches!(command.take(),Some(SftpClientCommand::Workspace(_,ref c)) if c.operation==SftpOperation::FileInspect)
    );
    let run = panel
        .browser
        .operation
        .as_ref()
        .unwrap()
        .run
        .as_ref()
        .unwrap();
    assert_eq!(run.completed, 1);
    file.id = SessionId::new();
    file.draft = run.current.clone();
    file.state = SftpFileState::Unknown;
    file.detail = "permission denied".into();
    view.file_job = Some(std::sync::Arc::new(file));
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none());
    assert!(
        panel
            .browser
            .operation
            .as_ref()
            .unwrap()
            .error
            .as_ref()
            .unwrap()
            .contains("1/4")
    );
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none());
}
#[test]
fn failed_inspect_is_detected_even_if_an_unrelated_poll_cleared_its_error() {
    let (mut panel, mut view, session) = populated();
    submit(&mut panel, &view, session, SftpFileAction::Rename);
    view.generation += 1;
    view.error = None;
    let mut command = None;
    panel.advance_file_operation(session, &view, &mut command);
    assert!(command.is_none());
    assert!(panel.browser.operation.as_ref().unwrap().error.is_some());
}
#[test]
fn windows_local_permissions_are_read_only_and_keep_actual_file_attributes() {
    if !cfg!(windows) {
        return;
    }
    let ctx = context();
    let (mut panel, view, _) = fixture();
    panel.browser.selection[0]
        .paths
        .insert(view.local_entries[0].path.clone());
    panel.open_file_dialog(&view, true, SftpFileAction::Inspect);
    for _ in 0..3 {
        frame(&ctx, &mut panel, &view, vec![], Modifiers::NONE);
    }
    let (tree, _) = frame(&ctx, &mut panel, &view, vec![], Modifiers::NONE);
    assert_eq!(
        panel.browser.operation.as_ref().unwrap().action,
        SftpFileAction::Inspect
    );
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, n)| n.role() == accesskit::Role::Button && n.label() == Some("应用"))
    );
    assert!(tree.nodes.iter().any(|(_, n)| {
        n.label()
            .or(n.value())
            .is_some_and(|text| text.contains("ACL"))
    }));
}

#[test]
fn failed_status_poll_releases_modal_instead_of_waiting_forever() {
    for state in [SftpFileState::Inspecting, SftpFileState::Applying] {
        let (mut panel, mut view, session) = populated();
        let draft = submit(&mut panel, &view, session, SftpFileAction::Rename);
        view.file_job = Some(std::sync::Arc::new(job(&view, session, draft, state)));
        view.generation += 1;
        view.error = Some("SSH session unavailable".into());
        let mut command = None;
        panel.advance_file_operation(session, &view, &mut command);
        assert!(command.is_none());
        assert!(
            panel
                .browser
                .operation
                .as_ref()
                .unwrap()
                .error
                .as_ref()
                .unwrap()
                .contains("停止后续提交")
        );
    }
}
