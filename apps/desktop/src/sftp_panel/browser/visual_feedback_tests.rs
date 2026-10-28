use super::tests::{click, draw, fixture};
use super::*;
use crate::ui_layout_gate::{bounds, check_window, context};
use egui::{Event, PointerButton, Pos2, accesskit};
fn pointer(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Default::default(),
    }
}
fn button(tree: &accesskit::TreeUpdate, label: &str) -> Pos2 {
    bounds(
        &tree
            .nodes
            .iter()
            .find(|(_, n)| n.role() == accesskit::Role::Button && n.label() == Some(label))
            .unwrap()
            .1,
    )
    .center()
}
#[test]
fn current_peer_changes_with_session_and_advanced_features_stay_in_menu() {
    let ctx = context();
    let (mut panel, mut view, _) = fixture();
    let size = egui::vec2(720.0, 480.0);
    let peer = cshell_domain::ClipboardTrust {
        host: cshell_domain::ClipboardHost::new("2001:db8::1", 2222).unwrap(),
        username: "operator".into(),
        host_key_sha256: "test-key".into(),
        route: "direct".into(),
    };
    view.peer = Some(peer);
    for _ in 0..4 {
        draw(&ctx, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&ctx, &mut panel, &view, size, vec![]);
    check_window(&ctx, size, "SFTP 文件", &tree);
    assert!(tree.nodes.iter().any(|(_, n)| {
        n.label()
            .or(n.value())
            .is_some_and(|s| s.contains("operator@[2001:db8::1]:2222"))
    }));
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, n)| n.label() == Some("历史 / 迁移"))
    );
    click(&ctx, &mut panel, &view, size, "更多功能");
    for _ in 0..3 {
        draw(&ctx, &mut panel, &view, size, vec![]);
    }
    let (menu_tree, _) = draw(&ctx, &mut panel, &view, size, vec![]);
    for label in ["队列详情", "远程编辑", "历史 / 迁移"] {
        let target = button(&menu_tree, label);
        assert!(
            egui::Rect::from_min_size(Pos2::ZERO, size).contains(target),
            "menu {label} outside viewport: {target:?}"
        );
    }

    click(&ctx, &mut panel, &view, size, "远程编辑");
    let (diagnostic, _) = draw(&ctx, &mut panel, &view, size, vec![]);
    assert_eq!(
        panel.page,
        2,
        "visible buttons: {:?}",
        diagnostic
            .nodes
            .iter()
            .filter(|(_, n)| n.role() == accesskit::Role::Button)
            .map(|(_, n)| (n.label(), bounds(n)))
            .collect::<Vec<_>>()
    );
    click(&ctx, &mut panel, &view, size, "文件与传输");
    assert_eq!(panel.page, 0);
    view.session_id = Some(SessionId::new());
    view.peer = None;
    let (tree, _) = draw(&ctx, &mut panel, &view, size, vec![]);
    assert!(!tree.nodes.iter().any(|(_, n)| {
        n.label()
            .or(n.value())
            .is_some_and(|s| s.contains("2001:db8::1"))
    }));
}
#[test]
fn drag_prompt_names_the_direction_and_current_target_before_immediate_transfer() {
    let ctx = context();
    let (mut panel, view, session) = fixture();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..3 {
        draw(&ctx, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&ctx, &mut panel, &view, size, vec![]);
    let from = button(&tree, "upload.txt");
    draw(
        &ctx,
        &mut panel,
        &view,
        size,
        vec![Event::PointerMoved(from), pointer(from, true)],
    );
    draw(
        &ctx,
        &mut panel,
        &view,
        size,
        vec![Event::PointerMoved(from + egui::vec2(15.0, 15.0))],
    );
    let invalid = panel.browser.drop_feedback[0].as_ref().unwrap();
    assert!(!invalid.allowed);
    assert!(invalid.caption.contains("远端"));
    let to = panel.browser.remote_rect.unwrap().center_bottom() - egui::vec2(0.0, 30.0);
    draw(&ctx, &mut panel, &view, size, vec![Event::PointerMoved(to)]);
    let feedback = panel.browser.drop_feedback[1].as_ref().unwrap();
    assert!(feedback.allowed);
    assert!(feedback.caption.contains("上传"));
    assert_eq!(feedback.path, "/home/operator");
    let (_, command) = draw(&ctx, &mut panel, &view, size, vec![pointer(to, false)]);
    assert!(
        matches!(command,Some(SftpClientCommand::TransferBatch(id,drafts,_,true)) if id==session&&drafts.len()==1)
    );
}
#[test]
fn unloaded_or_foreign_drop_target_is_rejected_without_staging() {
    for (local, source, same, count) in [
        (false, true, false, 1),
        (false, true, true, 129),
        (true, false, true, 0),
        (true, true, true, 1),
    ] {
        let feedback = visuals::DropFeedback::new(local, Some("/tmp"), source, same, count);
        assert!(!feedback.allowed);
    }
    let ctx = context();
    let (mut panel, mut view, _) = fixture();
    let size = egui::vec2(1280.0, 800.0);
    view.remote_loaded_directory = None;
    view.entries.clear();
    for _ in 0..3 {
        draw(&ctx, &mut panel, &view, size, vec![]);
    }
    let (tree, _) = draw(&ctx, &mut panel, &view, size, vec![]);
    let from = button(&tree, "upload.txt");
    draw(
        &ctx,
        &mut panel,
        &view,
        size,
        vec![Event::PointerMoved(from), pointer(from, true)],
    );
    draw(
        &ctx,
        &mut panel,
        &view,
        size,
        vec![Event::PointerMoved(from + egui::vec2(15.0, 15.0))],
    );
    let to = panel.browser.remote_rect.unwrap().center_bottom() - egui::vec2(0.0, 30.0);
    draw(&ctx, &mut panel, &view, size, vec![Event::PointerMoved(to)]);
    assert!(!panel.browser.drop_feedback[1].as_ref().unwrap().allowed);
    assert!(
        draw(&ctx, &mut panel, &view, size, vec![pointer(to, false)])
            .1
            .is_none()
    );
    assert!(panel.browser.staged.is_empty());
}
#[test]
fn native_file_hover_highlights_upload_only_on_remote_pane() {
    let ctx = context();
    let (mut panel, view, _) = fixture();
    let size = egui::vec2(1280.0, 800.0);
    for _ in 0..3 {
        draw(&ctx, &mut panel, &view, size, vec![]);
    }
    for (side, rect) in [
        (0, panel.browser.local_rect.unwrap()),
        (1, panel.browser.remote_rect.unwrap()),
    ] {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(Pos2::ZERO, size)),
                events: vec![Event::PointerMoved(rect.center())],
                hovered_files: vec![egui::HoveredFile {
                    path: Some("C:/upload.bin".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
            |ui| {
                panel.draw(ui.ctx(), view.session_id, Some(&view));
            },
        );
        output.textures_delta.clear();
        assert_eq!(
            panel.browser.drop_feedback[side].as_ref().unwrap().allowed,
            side == 1
        );
    }
}

#[test]
fn standalone_toolbar_disconnect_reconnect_and_busy_states_are_explicit() {
    use crate::sftp_connection::ConnectionPhase;
    let ctx=context();let (mut panel,mut view,session)=fixture();let profile=cshell_domain::ProfileId::new();
    view.connection_profile=Some(profile);view.connection_phase=ConnectionPhase::Connected;
    let size=egui::vec2(720.0,480.0);
    for _ in 0..4 {draw(&ctx,&mut panel,&view,size,vec![]);}
    let (tree,_)=draw(&ctx,&mut panel,&view,size,vec![]);
    for label in ["断开","重新连接","更多功能"] {assert!(egui::Rect::from_min_size(Pos2::ZERO,size).contains(button(&tree,label)));}
    assert!(matches!(click(&ctx,&mut panel,&view,size,"断开"),Some(SftpClientCommand::Disconnect(id)) if id==session));
    view.connection_phase=ConnectionPhase::Disconnected;view.status="SFTP 已断开；可点击重新连接".into();
    let (tree,_)=draw(&ctx,&mut panel,&view,size,vec![]);
    assert!(tree.nodes.iter().any(|(_,n)|n.label().or(n.value()).is_some_and(|s|s.contains("SFTP 已断开"))));
    assert!(click(&ctx,&mut panel,&view,size,"断开").is_none());
    assert!(matches!(click(&ctx,&mut panel,&view,size,"重新连接"),Some(SftpClientCommand::ConnectProfile{profile:p,previous:Some(old),session:new,..}) if p==profile && old==session && new!=session));
    view.connection_phase=ConnectionPhase::Connecting;
    assert!(click(&ctx,&mut panel,&view,size,"重新连接").is_none());
}

#[test]
fn remote_host_tabs_are_on_the_right_and_select_close_and_add_use_stable_ids() {
    use crate::sftp_panel::{RemoteTab,TabAction};use crate::sftp_connection::ConnectionPhase;
    let ctx=context();let (mut panel,mut view,_)=fixture();let size=egui::vec2(900.0,600.0);
    panel.remote_tabs=vec![RemoteTab{id:41,title:"主机 A".into(),host:"alice@a:22".into(),phase:ConnectionPhase::Connected,active:true,running:1},RemoteTab{id:73,title:"主机 B".into(),host:"bob@b:22".into(),phase:ConnectionPhase::Disconnected,active:false,running:0}];
    let profile=cshell_domain::ProfileId::new();panel.available_hosts=vec![(profile,"新增主机 C".into(),"user@c:22".into())];
    for _ in 0..4 {draw(&ctx,&mut panel,&view,size,vec![]);}
    let (tree,_)=draw(&ctx,&mut panel,&view,size,vec![]);
    for label in ["SFTP 标签 · 主机 A","SFTP 标签 · 主机 B","新建 SFTP 标签"] {assert!(panel.browser.remote_rect.unwrap().contains(button(&tree,label)),"tabs must stay on the remote side: {label}");}
    assert!(click(&ctx,&mut panel,&view,size,"SFTP 标签 · 主机 B").is_none());assert_eq!(panel.take_tab_action(),Some(TabAction::Select(73)));
    click(&ctx,&mut panel,&view,size,"关闭 SFTP 标签 · 主机 B");assert_eq!(panel.take_tab_action(),Some(TabAction::Close(73)));
    click(&ctx,&mut panel,&view,size,"新建 SFTP 标签");for _ in 0..3 {draw(&ctx,&mut panel,&view,size,vec![]);}
    click(&ctx,&mut panel,&view,size,"新增主机 C");assert_eq!(panel.take_tab_action(),Some(TabAction::Open(profile)));
    view.connection_profile=Some(profile);view.connection_phase=ConnectionPhase::Disconnected;view.status="SFTP 已断开".into();
    let (tree,_)=draw(&ctx,&mut panel,&view,size,vec![]);assert!(tree.nodes.iter().any(|(_,n)|n.label()==Some("SFTP 标签 · 主机 A")));assert!(panel.browser.local_rect.is_some());
}

#[test]
fn shared_ssh_toolbar_keeps_status_and_connection_controls_inside_small_window() {
    let ctx=context();let(mut panel,mut view,session)=fixture();let profile=cshell_domain::ProfileId::new();
    let peer=cshell_domain::ClipboardTrust{host:cshell_domain::ClipboardHost::new("mfa.example.test",22).unwrap(),username:"operator".into(),host_key_sha256:"verified-test-key".into(),route:"direct".into()};
    view.peer=Some(peer.clone());view.connection_profile=Some(profile);view.connection_phase=crate::sftp_connection::ConnectionPhase::Connected;
    view.connection_info=Some(cshell_application::SftpConnectionInfo{session,profile,title:"MFA host".into(),peer,running:true,shared_from:Some(SessionId::new()),initial_directory:None});
    let size=egui::vec2(720.0,480.0);
    for page in [0,1] {
        panel.page=page;for _ in 0..3{draw(&ctx,&mut panel,&view,size,vec![]);}
        let(tree,_)=draw(&ctx,&mut panel,&view,size,vec![]);
        assert!(tree.nodes.iter().any(|(_,n)|n.label().or(n.value())==Some("复用 SSH")));
        for label in ["断开","重新连接","更多功能"] {let pos=button(&tree,label);assert!(egui::Rect::from_min_size(Pos2::ZERO,size).contains(pos),"{label}: {pos:?}");}
    }
}
