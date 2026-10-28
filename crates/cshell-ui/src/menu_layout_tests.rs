use super::*;
use crate::tests::{button_center, classic_context};

fn frame(
    context: &egui::Context,
    model: &mut WorkbenchViewModel,
    size: egui::Vec2,
    events: Vec<egui::Event>,
) -> egui::accesskit::TreeUpdate {
    let mut output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            ..Default::default()
        },
        |ui| {
            let _ = draw_workbench(ui, model);
        },
    );
    output.textures_delta.clear();
    output
        .platform_output
        .accesskit_update
        .expect("accessible menus")
}

fn click(
    context: &egui::Context,
    model: &mut WorkbenchViewModel,
    size: egui::Vec2,
    pos: egui::Pos2,
) {
    frame(
        context,
        model,
        size,
        vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ],
    );
}

#[test]
fn top_menus_and_nested_actions_fit_and_dispatch() {
    for size in [egui::vec2(900.0, 600.0), egui::vec2(720.0, 480.0)] {
        for path in [
            vec!["文件"],
            vec!["编辑"],
            vec!["查看"],
            vec!["会话"],
            vec!["帮助"],
            vec!["分屏"],
            vec!["工具", "高级设置"],
            vec!["工具", "批量文件与混合任务"],
        ] {
            let context = classic_context();
            context.enable_accesskit();
            let mut model = WorkbenchViewModel::default();
            frame(&context, &mut model, size, vec![]);
            let mut tree = frame(&context, &mut model, size, vec![]);
            for label in &path {
                let node = tree
                    .nodes
                    .iter()
                    .find(|(_, n)| {
                        n.label()
                            .is_some_and(|s| s.trim_end_matches('⏵').trim_end() == *label)
                    })
                    .unwrap_or_else(|| panic!("missing {label}"));
                let b = node.1.bounds().expect("menu button bounds");
                let pos = egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32);
                click(&context, &mut model, size, pos);
                for _ in 0..2 {
                    tree = frame(&context, &mut model, size, vec![]);
                }
            }
            for (_, node) in &tree.nodes {
                if node.role() != egui::accesskit::Role::Button {
                    continue;
                }
                if let Some(b) = node.bounds() {
                    assert!(
                        b.x0 >= -1.0
                            && b.y0 >= -1.0
                            && b.x1 <= f64::from(size.x) + 1.0
                            && b.y1 <= f64::from(size.y) + 1.0,
                        "{path:?}: {:?} outside {size:?}: {b:?}",
                        node.label()
                    );
                }
            }
            let leaf = match path.last().copied() {
                Some("高级设置") => Some(("SSH 能力…", WorkbenchMenuCommand::SshProviders)),
                Some("批量文件与混合任务") => {
                    Some(("多主机上传", WorkbenchMenuCommand::UploadTask))
                }
                _ => None,
            };
            if let Some((label, expected)) = leaf {
                let pos = button_center(&tree, label);
                click(&context, &mut model, size, pos);
                assert_eq!(model.menu_command, Some(expected));
            }
        }
    }
}

#[test]
fn ssh_toolbar_controls_fit_dispatch_and_block_repeated_operations() {
    for size in [
        egui::vec2(720.0, 480.0),
        egui::vec2(900.0, 600.0),
        egui::vec2(1280.0, 800.0),
    ] {
        let context = classic_context();
        context.enable_accesskit();
        let mut model = WorkbenchViewModel {
            can_disconnect_ssh: true,
            can_restart_ssh: true,
            ..Default::default()
        };
        for _ in 0..2 {
            frame(&context, &mut model, size, vec![]);
        }
        for (label, command) in [
            ("断开", WorkbenchMenuCommand::DisconnectSsh),
            ("重新连接", WorkbenchMenuCommand::ReconnectSsh),
        ] {
            let tree = frame(&context, &mut model, size, vec![]);
            let node = &tree
                .nodes
                .iter()
                .find(|(_, n)| {
                    n.label() == Some(label) && n.role() == egui::accesskit::Role::Button
                })
                .unwrap()
                .1;
            let bounds = node.bounds().unwrap();
            assert!(
                bounds.x0 >= 0.0 && bounds.x1 <= f64::from(size.x),
                "{label} outside {size:?}: {bounds:?}"
            );
            let position = button_center(&tree, label);
            model.menu_command = None;
            click(&context, &mut model, size, position);
            assert_eq!(model.menu_command, Some(command));
            model.menu_command = None;
            model.ssh_control_busy = true;
            let busy = frame(&context, &mut model, size, vec![]);
            let position = button_center(&busy, label);
            click(&context, &mut model, size, position);
            assert_eq!(model.menu_command, None, "busy controls must not dispatch");
            model.ssh_control_busy = false;
        }
        for label in ["断开", "重新连接"] {
            model.can_disconnect_ssh = false;
            model.can_restart_ssh = false;
            model.menu_command = None;
            let tree = frame(&context, &mut model, size, vec![]);
            let position = button_center(&tree, label);
            click(&context, &mut model, size, position);
            assert_eq!(model.menu_command, None);
        }
    }
}

#[test]
fn disconnected_ssh_is_visible_in_tab_banner_and_status_and_reconnect_targets_that_tab() {
    for size in [
        egui::vec2(720.0, 480.0),
        egui::vec2(900.0, 600.0),
        egui::vec2(1280.0, 800.0),
    ] {
        let context = classic_context();
        context.enable_accesskit();
        let tab = cshell_domain::TabId::new();
        let mut model = WorkbenchViewModel {
            selected_tab: Some(tab),
            can_restart_ssh: true,
            tabs: vec![WorkspaceTabViewModel {
                id: tab,
                title: "operator@server.example.test".into(),
                session_id: None,
            }],
            ..Default::default()
        };
        for state in [
            SshConnectionState::Disconnecting,
            SshConnectionState::Reconnecting,
            SshConnectionState::Unconfirmed,
            SshConnectionState::Disconnected,
            SshConnectionState::Connected,
        ] {
            model.ssh_tab_states.insert(tab, state);
            model.ssh_control_busy = matches!(
                state,
                SshConnectionState::Disconnecting | SshConnectionState::Reconnecting
            );
            model.tabs[0].session_id = if state == SshConnectionState::Disconnected {
                None
            } else {
                Some(cshell_domain::SessionId::new())
            };
            for _ in 0..2 {
                frame(&context, &mut model, size, vec![]);
            }
            let tree = frame(&context, &mut model, size, vec![]);
            let visible = tree
                .nodes
                .iter()
                .map(|(_, n)| format!("{} {}", n.label().unwrap_or(""), n.value().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(visible.contains(state.label()), "missing state {state:?}");
            assert_eq!(
                visible.contains("SSH 已断开"),
                state == SshConnectionState::Disconnected
            );
            if state == SshConnectionState::Disconnected {
                assert!(visible.contains("输入不会发送到服务器"));
                assert!(
                    tree.nodes
                        .iter()
                        .any(|(_, n)| n.role() == egui::accesskit::Role::Label
                            && (n.value() == Some("已断开") || n.label() == Some("已断开"))),
                    "tab must have a visible badge"
                );
                let button = tree
                    .nodes
                    .iter()
                    .filter(|(_, n)| {
                        n.role() == egui::accesskit::Role::Button && n.label() == Some("重新连接")
                    })
                    .max_by(|(_, a), (_, b)| {
                        a.bounds().unwrap().y0.total_cmp(&b.bounds().unwrap().y0)
                    })
                    .unwrap();
                let b = button.1.bounds().unwrap();
                assert!(b.y0 > 100.0, "use the terminal banner button");
                assert!(b.x0 >= 0.0 && b.x1 <= f64::from(size.x) && b.y1 <= f64::from(size.y));
                model.menu_command = None;
                click(
                    &context,
                    &mut model,
                    size,
                    egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32),
                );
                assert_eq!(
                    model.menu_command,
                    Some(WorkbenchMenuCommand::ReconnectSshTab(tab))
                );
            }
        }
    }
}


#[test]
fn toolbar_keeps_core_actions_in_bounds_at_supported_widths_and_scales() {
    for scale in [1.0, 1.25, 1.5, 2.0] {
        for width in [720.0,900.0,1280.0] {
            let context = classic_context(); context.enable_accesskit(); context.set_pixels_per_point(scale);
            let size=egui::vec2(width,600.0);
            let mut model=WorkbenchViewModel::default();
            for _ in 0..3 { frame(&context,&mut model,size,vec![]); }
            let tree=frame(&context,&mut model,size,vec![]);
            let mut previous=0.0;
            for label in ["＋ 新建","会话管理","连接","断开","重新连接","SFTP","分屏","更多"] {
                let node=&tree.nodes.iter().find(|(_,n)|n.role()==egui::accesskit::Role::Button && n.label()==Some(label)).unwrap().1;
                let b=node.bounds().unwrap();
                assert!(b.x0>=previous && b.x1<=f64::from(width)+1.0,"{label} overlaps or leaves {width} at {scale}: {b:?}");
                previous=b.x1;
            }
        }
    }
}

#[test]
fn wide_toolbar_and_compact_more_copy_the_current_authenticated_channel() {
    for width in [720.0,1280.0] {
        let context=classic_context();context.enable_accesskit();let size=egui::vec2(width,600.0);
        let tab=cshell_domain::TabId::new();let session=cshell_domain::SessionId::new();
        let mut model=WorkbenchViewModel{selected_tab:Some(tab),tabs:vec![WorkspaceTabViewModel{id:tab,title:"SSH".into(),session_id:Some(session)}],reusable_ssh_sessions:vec![session],..Default::default()};
        for _ in 0..2 {frame(&context,&mut model,size,vec![]);}
        let mut tree=frame(&context,&mut model,size,vec![]);
        let label=if width<1080.0 {
            let pos=button_center(&tree,"更多");click(&context,&mut model,size,pos);
            for _ in 0..2 {tree=frame(&context,&mut model,size,vec![]);}
            "复制 SSH 渠道"
        }else{"复制渠道"};
        let pos=button_center(&tree,label);click(&context,&mut model,size,pos);
        assert_eq!(model.menu_command,Some(WorkbenchMenuCommand::CloneSshChannel(tab)));
    }
}

#[test]
fn each_split_identity_follows_its_own_binding_not_sidebar_selection() {
    use cshell_domain::*;
    let context=classic_context();context.enable_accesskit();let size=egui::vec2(1280.0,800.0);
    let mut doc=WorkspaceDocument::default();let original=doc.tab_groups[0].id;
    let other=TabGroupId::new();let first=PaneId::new();let second=PaneId::new();
    doc.panes[0].content=WorkspacePaneContent::Split{axis:SplitAxis::Horizontal,ratio_permille:500,first,second};
    doc.panes.extend([WorkspacePane{id:first,content:WorkspacePaneContent::Tabs{group_id:original}},WorkspacePane{id:second,content:WorkspacePaneContent::Tabs{group_id:other}}]);
    doc.tab_groups.push(WorkspaceTabGroup{id:other,tabs:vec![],active_tab:None});
    let mut model=WorkbenchViewModel::default();
    for (index,target) in ["deploy@first.example:22","admin@[2001:db8::2]:2222"].into_iter().enumerate() {
        let tab=TabId::new();let profile=ProfileId::new();
        doc.bindings.push(WorkspaceBinding{tab_id:tab,profile_id:Some(profile),title:format!("server-{index}")});
        doc.tab_groups[index].tabs.push(tab);doc.tab_groups[index].active_tab=Some(tab);
        model.saved_ssh_profiles.push(SavedSshProfileViewModel{id:profile,name:format!("server-{index}"),target:target.into(),favorite:false,folder_id:None,kind:ProfileKind::Ssh});
        model.ssh_tab_states.insert(tab,SshConnectionState::Connected);
        if index==0 {
            model.selected_tab=Some(tab);model.tabs.push(WorkspaceTabViewModel{id:tab,title:"server-0".into(),session_id:Some(SessionId::new())});
        }else{model.selected_profile=Some(profile);}
    }
    model.workspace_window_id=Some(doc.windows[0].id);model.workspace_document=Some(doc);
    for _ in 0..2 {frame(&context,&mut model,size,vec![]);}
    let tree=frame(&context,&mut model,size,vec![]);
    let text=tree.nodes.iter().map(|(_,n)|format!("{} {}",n.label().unwrap_or(""),n.value().unwrap_or(""))).collect::<Vec<_>>().join("\n");
    for target in ["配置：deploy@first.example:22","配置：admin@[2001:db8::2]:2222"] {assert!(text.contains(target),"missing {target}: {text}");}
    assert!(text.contains("当前输入"));assert_eq!(model.pane_views.len(),2);
    for pane in &model.pane_views {
        assert!(pane.rect.width()>100.0 && pane.rect.height()>100.0);
        let target=if pane.tab_id==model.selected_tab {"配置：deploy@first.example:22"}else{"配置：admin@[2001:db8::2]:2222"};
        let b=tree.nodes.iter().find(|(_,n)|n.value()==Some(target)||n.label()==Some(target)).unwrap().1.bounds().unwrap();
        assert!(b.x0>=f64::from(pane.rect.min.x)-1.0 && b.x1<=f64::from(pane.rect.max.x)+1.0);
        assert!(b.y1<=f64::from(pane.rect.min.y)+1.0,"host identity must not cover GPU terminal");
    }
}


#[test]
fn ssh_chrome_leaves_the_gpu_terminal_canvas_transparent() {
    let context=classic_context();let tab=cshell_domain::TabId::new();
    let mut doc=cshell_domain::WorkspaceDocument::default();
    doc.tab_groups[0].tabs.push(tab);doc.tab_groups[0].active_tab=Some(tab);
    doc.bindings.push(cshell_domain::WorkspaceBinding{tab_id:tab,profile_id:None,title:"GPU terminal".into()});
    let mut model=WorkbenchViewModel{workspace_window_id:Some(doc.windows[0].id),workspace_document:Some(doc),selected_tab:Some(tab),..Default::default()};
    model.ssh_tab_states.insert(tab,SshConnectionState::Connected);
    let mut output=context.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(1280.0,800.0))),..Default::default()},|ui|{let _=draw_workbench(ui,&mut model);});
    output.textures_delta.clear();let body=model.pane_views[0].rect;
    fn check(shape:&egui::Shape,body:egui::Rect) {
        match shape {
            egui::Shape::Rect(rect)=>assert!(!(rect.fill.a()>0 && rect.rect.contains(body.center()) && rect.rect.width()>=body.width()*0.8 && rect.rect.height()>=body.height()*0.8),"opaque workbench layer would hide the GPU terminal"),
            egui::Shape::Vec(shapes)=>for shape in shapes {check(shape,body);},
            _=>{}
        }
    }
    for shape in &output.shapes {check(&shape.shape,body);}
}
