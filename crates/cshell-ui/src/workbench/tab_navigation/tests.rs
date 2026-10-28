use super::*;
use cshell_domain::*;

fn fixture(count: usize) -> WorkbenchViewModel {
    let mut model = WorkbenchViewModel {
        sidebar_hidden: true,
        ..Default::default()
    };
    let mut doc = WorkspaceDocument::default();
    for index in 0..count {
        let tab = TabId::new();
        let profile = ProfileId::new();
        let title = format!("server-{index:02}");
        doc.bindings.push(WorkspaceBinding {
            tab_id: tab,
            profile_id: Some(profile),
            title: title.clone(),
        });
        doc.tab_groups[0].tabs.push(tab);
        model.tabs.push(WorkspaceTabViewModel {
            id: tab,
            title: title.clone(),
            session_id: Some(SessionId::new()),
        });
        model.saved_ssh_profiles.push(SavedSshProfileViewModel {
            id: profile,
            name: title,
            target: format!("root@host-{index:02}.example:22"),
            favorite: index == 0,
            folder_id: None,
            kind: ProfileKind::Ssh,
        });
        model
            .ssh_tab_states
            .insert(tab, SshConnectionState::Connected);
    }
    doc.tab_groups[0].active_tab = model.tabs.first().map(|t| t.id);
    model.selected_tab = doc.tab_groups[0].active_tab;
    model.workspace_window_id = Some(doc.windows[0].id);
    model.workspace_document = Some(doc);
    model
}
fn frame(
    context: &egui::Context,
    model: &mut WorkbenchViewModel,
    width: f32,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    let mut out = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 600.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            let _ = draw_workbench(ui, model);
        },
    );
    out.textures_delta.clear();
    out
}
fn key(key: egui::Key) -> Vec<egui::Event> {
    vec![
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        },
    ]
}
#[test]
fn navigator_includes_other_splits_but_never_other_windows() {
    let mut model = fixture(20);
    let first = model.workspace_document.as_ref().unwrap().tab_groups[0].id;
    let doc = model.workspace_document.as_mut().unwrap();
    let second = TabGroupId::new();
    let a = PaneId::new();
    let b = PaneId::new();
    let moved = doc.tab_groups[0].tabs.pop().unwrap();
    doc.panes[0].content = WorkspacePaneContent::Split {
        axis: SplitAxis::Horizontal,
        ratio_permille: 500,
        first: a,
        second: b,
    };
    doc.panes.extend([
        WorkspacePane {
            id: a,
            content: WorkspacePaneContent::Tabs { group_id: first },
        },
        WorkspacePane {
            id: b,
            content: WorkspacePaneContent::Tabs { group_id: second },
        },
    ]);
    doc.tab_groups.push(WorkspaceTabGroup {
        id: second,
        tabs: vec![moved],
        active_tab: Some(moved),
    });
    let foreign = TabId::new();
    let other = TabGroupId::new();
    let root = PaneId::new();
    doc.panes.push(WorkspacePane {
        id: root,
        content: WorkspacePaneContent::Tabs { group_id: other },
    });
    doc.tab_groups.push(WorkspaceTabGroup {
        id: other,
        tabs: vec![foreign],
        active_tab: Some(foreign),
    });
    doc.windows.push(WorkspaceWindow {
        id: WorkspaceWindowId::new(),
        root,
        focused_group: other,
    });
    doc.bindings.push(WorkspaceBinding {
        tab_id: foreign,
        profile_id: None,
        title: "other window".into(),
    });
    let list = entries(&model);
    assert_eq!(list.len(), 20);
    assert_eq!(list.last().unwrap().tab, moved);
    assert_eq!(list.last().unwrap().group, 2);
    assert!(!list.iter().any(|e| e.tab == foreign));
}
#[test]
fn query_keyboard_switch_and_escape_never_launch_or_reconnect() {
    let context = crate::tests::classic_context();
    context.enable_accesskit();
    let mut model = fixture(20);
    let first = model.selected_tab;
    let last = model.tabs[19].id;
    let sessions: Vec<_> = model.tabs.iter().map(|t| t.session_id).collect();
    model.tab_navigator.show(first);
    for _ in 0..2 {
        frame(&context, &mut model, 720.0, vec![]);
    }
    frame(
        &context,
        &mut model,
        720.0,
        vec![egui::Event::Text("HOST-19 root".into())],
    );
    assert_eq!(model.tab_navigator.selected, Some(last));
    frame(&context, &mut model, 720.0, key(egui::Key::Enter));
    assert_eq!(model.selected_tab, Some(last));
    assert!(!model.tab_navigator.open);
    assert!(model.tab_navigator.return_focus_requested);
    assert_eq!(model.menu_command, None);
    assert_eq!(
        sessions,
        model.tabs.iter().map(|t| t.session_id).collect::<Vec<_>>()
    );
    model.tab_navigator.show(Some(last));
    for _ in 0..2 {
        frame(&context, &mut model, 720.0, vec![]);
    }
    frame(&context, &mut model, 720.0, key(egui::Key::ArrowUp));
    assert_eq!(model.tab_navigator.selected, Some(model.tabs[18].id));
    frame(&context, &mut model, 720.0, key(egui::Key::Escape));
    assert_eq!(model.selected_tab, Some(last));
    assert!(!model.tab_navigator.open);
    assert!(model.tab_navigator.query.is_empty());
    assert!(model.menu_command.is_none());
}
#[test]
fn no_matches_enter_and_busy_state_never_switch() {
    let context = crate::tests::classic_context();
    let mut model = fixture(2);
    let first = model.selected_tab;
    model.tab_navigator.show(first);
    model.tab_navigator.query = "missing-server".into();
    frame(&context, &mut model, 720.0, vec![]);
    frame(&context, &mut model, 720.0, key(egui::Key::Enter));
    assert!(model.tab_navigator.open);
    assert_eq!(model.selected_tab, first);
    model.tab_navigator.query.clear();
    model.tab_navigator.selected = Some(model.tabs[1].id);
    model.ssh_control_busy = true;
    frame(&context, &mut model, 720.0, key(egui::Key::Enter));
    assert_eq!(model.selected_tab, first);
}
#[test]
fn selected_tab_is_revealed_after_switch_and_resize_without_forcing_manual_scroll() {
    let context = crate::tests::classic_context();
    context.enable_accesskit();
    let mut model = fixture(20);
    let last = model.tabs[19].id;
    model.selected_tab = Some(last);
    model.workspace_document.as_mut().unwrap().tab_groups[0].active_tab = Some(last);
    for width in [1280.0, 720.0] {
        let mut output = None;
        for _ in 0..8 {
            output = Some(frame(&context, &mut model, width, vec![]));
        }
        let tree = output.unwrap().platform_output.accesskit_update.unwrap();
        let label = tree
            .nodes
            .iter()
            .find(|(_, n)| {
                n.role() == egui::accesskit::Role::Button && n.label() == Some("server-19")
            })
            .unwrap()
            .1
            .bounds()
            .unwrap();
        assert!(
            label.x0 >= 0.0 && label.x1 < f64::from(width) - 70.0,
            "selected tab is outside visible strip: {label:?}"
        );
        let plus = tree
            .nodes
            .iter()
            .find(|(_, n)| n.role() == egui::accesskit::Role::Button && n.label() == Some("+"))
            .unwrap()
            .1
            .bounds()
            .unwrap();
        assert!(plus.x0 >= 0.0 && plus.x1 <= f64::from(width));
        frame(&context,&mut model,width,vec![egui::Event::PointerMoved(egui::pos2(120.0,82.0)),egui::Event::MouseWheel{phase:egui::TouchPhase::Move,unit:egui::MouseWheelUnit::Point,delta:egui::vec2(240.0,0.0),modifiers:egui::Modifiers::NONE}]);
        let mut manual=None;
        for _ in 0..12 {manual=Some(frame(&context,&mut model,width,vec![]));}
        let tree=manual.unwrap().platform_output.accesskit_update.unwrap();
        let moved=tree.nodes.iter().find(|(_,n)|n.role()==egui::accesskit::Role::Button && n.label()==Some("server-19")).unwrap().1.bounds().unwrap();
        assert!(moved.x0>label.x0+50.0,"manual tab scrolling must not snap back to the selected tab: {moved:?} vs {label:?}");

    }
}
#[test]
fn sidebar_tokens_match_name_host_and_username_and_enter_opens_one_profile() {
    assert!(search_matches(
        "PROD root",
        &["Production", "root@linux.example:22"]
    ));
    assert!(!search_matches(
        "prod admin",
        &["Production", "root@linux.example:22"]
    ));
    let context = crate::tests::classic_context();
    context.enable_accesskit();
    let mut model = fixture(20);
    model.sidebar_hidden = false;
    model.profile_search = "server-19 ROOT".into();
    let search_id = egui::Id::new("saved-session-search");
    for _ in 0..2 {
        frame(&context, &mut model, 900.0, vec![]);
    }
    context.memory_mut(|m| m.request_focus(search_id));
    frame(&context, &mut model, 900.0, key(egui::Key::Enter));
    assert_eq!(
        model.menu_command,
        Some(WorkbenchMenuCommand::OpenSavedProfile(
            model.saved_ssh_profiles[19].id
        ))
    );
    let parent=FolderId::new();let child=FolderId::new();
    model.profile_folders.extend([ProfileFolder{id:parent,name:"维护分组".into(),parent_id:None,terminal:Default::default()},ProfileFolder{id:child,name:"子目录".into(),parent_id:Some(parent),terminal:Default::default()}]);
    model.saved_ssh_profiles[19].folder_id=Some(child);
    model.profile_search="维护分组".into();model.selected_profile=None;model.menu_command=None;
    frame(&context,&mut model,900.0,vec![]);
    context.memory_mut(|m|m.request_focus(search_id));
    frame(&context,&mut model,900.0,key(egui::Key::Enter));
    assert_eq!(model.menu_command,Some(WorkbenchMenuCommand::OpenSavedProfile(model.saved_ssh_profiles[19].id)));

}

#[test]
fn navigator_fits_the_smallest_workbench_window() {
    for size in [egui::vec2(720.0,480.0),egui::vec2(900.0,600.0)] {
        let context=crate::tests::classic_context();context.enable_accesskit();let mut model=fixture(20);
        model.tab_navigator.show(model.selected_tab);
        let mut last=None;
        for _ in 0..3 {
            let mut out=context.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,size)),..Default::default()},|ui|{let _=draw_workbench(ui,&mut model);});
            out.textures_delta.clear();last=Some(out);
        }
        let rect=context.memory(|m|m.area_rect(egui::Id::new("ssh-tab-navigator"))).unwrap();
        assert!(egui::Rect::from_min_size(egui::Pos2::ZERO,size).expand(1.0).contains_rect(rect),"navigator outside {size:?}: {rect:?}");
        let tree=last.unwrap().platform_output.accesskit_update.unwrap();
        for (_,node) in &tree.nodes {
            if node.role()==egui::accesskit::Role::TextInput || node.label()==Some("关闭列表") {
                let b=node.bounds().unwrap();assert!(b.x0>=0.0 && b.y0>=0.0 && b.x1<=f64::from(size.x) && b.y1<=f64::from(size.y));
            }
        }
    }
}
