use super::*;
use crate::tests::classic_context;

#[test]
fn environment_identity_and_twenty_tabs_fit_split_panes_at_high_dpi() {
    use cshell_domain::*;
    for scale in [1.0, 1.25, 1.5, 2.0] {
        for width in [720.0, 900.0, 1280.0] {
            let ctx = classic_context();
            ctx.enable_accesskit();
            ctx.set_pixels_per_point(scale);
            let mut doc = WorkspaceDocument::default();
            let first = PaneId::new();
            let second = PaneId::new();
            let original = doc.tab_groups[0].id;
            let other = TabGroupId::new();
            doc.panes[0].content = WorkspacePaneContent::Split {
                axis: SplitAxis::Horizontal,
                ratio_permille: 500,
                first,
                second,
            };
            doc.panes.extend([
                WorkspacePane {
                    id: first,
                    content: WorkspacePaneContent::Tabs { group_id: original },
                },
                WorkspacePane {
                    id: second,
                    content: WorkspacePaneContent::Tabs { group_id: other },
                },
            ]);
            doc.tab_groups.push(WorkspaceTabGroup {
                id: other,
                tabs: vec![],
                active_tab: None,
            });
            let mut model = WorkbenchViewModel {
                sidebar_width: Some(160.0),
                ..Default::default()
            };
            for index in 0..20 {
                let tab = TabId::new();
                let profile = ProfileId::new();
                let group = index / 10;
                doc.bindings.push(WorkspaceBinding {
                    tab_id: tab,
                    profile_id: Some(profile),
                    title: format!("Linux 中文主机 / long-session-{index}"),
                });
                doc.tab_groups[group].tabs.push(tab);
                if index % 10 == 0 {
                    doc.tab_groups[group].active_tab = Some(tab);
                }
                model.saved_ssh_profiles.push(SavedSshProfileViewModel {
                    id: profile,
                    name: format!("Linux 中文主机 {index}"),
                    target: format!("deploy@[2001:db8::{index}]:2222"),
                    favorite: false,
                    folder_id: None,
                    kind: ProfileKind::Ssh,
                });
                model.profile_environments.insert(
                    profile,
                    if group == 0 {
                        environment::Environment::Production
                    } else {
                        environment::Environment::Development
                    },
                );
                model
                    .ssh_tab_states
                    .insert(tab, SshConnectionState::Connected);
                if index == 0 || index == 10 { model.input_target_tabs.insert(tab, index == 10); }
                if index == 10 { model.unread_output_tabs.insert(tab); }
                if index == 0 {
                    model.selected_tab = Some(tab);
                }
            }
            model.workspace_window_id = Some(doc.windows[0].id);
            model.workspace_document = Some(doc);
            let mut tree = None;
            for _ in 0..4 {
                let mut out = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 600.0),
                        )),
                        ..Default::default()
                    },
                    |ui| {
                        draw_workbench(ui, &mut model);
                    },
                );
                out.textures_delta.clear();
                tree = out.platform_output.accesskit_update;
            }
            assert_eq!(model.pane_views.len(), 2);
            let tree = tree.unwrap();
            for label in ["命令", "同步"] {
                assert!(tree.nodes.iter().any(|(_, n)| n.label() == Some(label) || n.value() == Some(label)), "missing recipient badge {label} at {scale}/{width}");
            }
            assert!(tree.nodes.iter().any(|(_, n)| n.label() == Some("有新输出") || n.value() == Some("有新输出")));
            for (index, label) in ["生产", "开发"].into_iter().enumerate() {
                let node = &tree
                    .nodes
                    .iter()
                    .find(|(_, n)| n.value() == Some(label) || n.label() == Some(label))
                    .unwrap()
                    .1;
                let b = node.bounds().unwrap();
                let pane = &model.pane_views[index];
                assert!(
                    b.x0 >= f64::from(pane.rect.min.x) - 1.0
                        && b.x1 <= f64::from(pane.rect.max.x) + 1.0,
                    "{label} / {scale} / {width}: {b:?}"
                );
                assert!(b.y1 <= f64::from(pane.rect.min.y) + 1.0);
                assert!(pane.rect.height() > 350.0 && pane.rect.width() > 200.0);
            }
        }
    }
}

#[test]
fn focus_layout_preserves_menu_access_and_restores_sidebar_and_toolbar() {
    let ctx = classic_context();
    ctx.enable_accesskit();
    let mut model = WorkbenchViewModel::default();
    let focus = workbench_layout::Layout {
        sidebar_hidden: true,
        toolbar_hidden: true,
        sidebar_width: Some(160.0),
    };
    focus.apply(&mut model);
    let mut out = ctx.run_ui(Default::default(), |ui| {
        draw_workbench(ui, &mut model);
    });
    out.textures_delta.clear();
    let tree = out.platform_output.accesskit_update.unwrap();
    assert!(tree.nodes.iter().any(|(_, n)| n.label() == Some("查看")));
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, n)| n.label() == Some("重新连接"))
    );
    fn render(ctx:&egui::Context,model:&mut WorkbenchViewModel,events:Vec<egui::Event>)->egui::accesskit::TreeUpdate {
        let mut out=ctx.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(900.0,600.0))),events,..Default::default()},|ui|{draw_workbench(ui,model);});
        out.textures_delta.clear();out.platform_output.accesskit_update.unwrap()
    }
    fn click_label(ctx:&egui::Context,model:&mut WorkbenchViewModel,label:&str){
        for _ in 0..3 {render(ctx,model,vec![]);}
        let tree=render(ctx,model,vec![]);
        let b=tree.nodes.iter().find(|(_,n)|n.label()==Some(label)).unwrap_or_else(||panic!("missing {label}")).1.bounds().unwrap();
        let pos=egui::pos2(((b.x0+b.x1)/2.0)as f32,((b.y0+b.y1)/2.0)as f32);
        for pressed in [true,false]{render(ctx,model,vec![egui::Event::PointerMoved(pos),egui::Event::PointerButton{pos,button:egui::PointerButton::Primary,pressed,modifiers:egui::Modifiers::NONE}]);}
    }
    click_label(&ctx,&mut model,"查看");
    click_label(&ctx,&mut model,"连接工具栏");
    if !egui::Popup::is_any_open(&ctx){click_label(&ctx,&mut model,"查看");}
    click_label(&ctx,&mut model,"会话管理器");
    assert!(!model.sidebar_hidden && !model.toolbar_hidden);
    workbench_layout::Layout::default().apply(&mut model);
    assert!(!model.sidebar_hidden && !model.toolbar_hidden && model.sidebar_width.is_none());
}

#[test]
fn readable_text_and_state_colors_have_contrast_on_workbench_surfaces() {
    fn luminance(c: egui::Color32) -> f64 {
        let a = c.to_array();
        [0.2126, 0.7152, 0.0722]
            .into_iter()
            .enumerate()
            .map(|(i, w)| {
                let v = f64::from(a[i]) / 255.0;
                w * if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            })
            .sum()
    }
    for fg in [
        theme::TEXT,
        theme::MUTED,
        theme::ACCENT,
        theme::SUCCESS,
        theme::WARNING,
        theme::DANGER,
    ] {
        for bg in [
            theme::BACKGROUND,
            theme::PANEL,
            theme::RAISED,
            theme::SELECTED,
        ] {
            let a = luminance(fg);
            let b = luminance(bg);
            assert!(
                (a.max(b) + 0.05) / (a.min(b) + 0.05) >= 4.5,
                "{fg:?} over {bg:?}"
            );
        }
    }
    let ctx = classic_context();
    assert_eq!(
        ctx.style_of(egui::Theme::Dark).visuals.weak_text_color(),
        theme::MUTED
    );
}
