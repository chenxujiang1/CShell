use super::*;
use crate::ui_layout_gate::{bounds, check_window, context, frame};

#[test]
fn ssh_connection_options_have_new_defaults_toggle_save_and_reload() {
    let context = context();
    let mut panel = panel();
    panel.editor_tab = 5;
    panel.profile_draft.as_mut().unwrap().record.name = "Recovery profile".into();
    let defaults = &panel.profile_draft.as_ref().unwrap().ssh_options;
    assert!(defaults.keepalive_enabled && defaults.auto_reconnect);
    assert_eq!(defaults.keepalive_interval_secs, 15);
    assert_eq!((defaults.reconnect_delay_secs, defaults.reconnect_attempts), (5, 3));
    fn render(context: &egui::Context, panel: &mut ProfilePanel, events: Vec<egui::Event>)
        -> (egui::accesskit::TreeUpdate, Option<ProfileClientCommand>)
    {
        let mut command = None;
        let mut output = context.run_ui(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 760.0))),
            events, ..Default::default()
        }, |ui| command = panel.draw(ui.ctx()));
        output.textures_delta.clear();
        (output.platform_output.accesskit_update.unwrap(), command)
    }
    fn click(context: &egui::Context, panel: &mut ProfilePanel, label: &str) -> Option<ProfileClientCommand> {
        let (tree, _) = render(context, panel, vec![]);
        let pos = bounds(&tree.nodes.iter().find(|(_, n)| n.label() == Some(label)).unwrap().1).center();
        render(context, panel, vec![egui::Event::PointerMoved(pos), egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE,
        }]);
        render(context, panel, vec![egui::Event::PointerButton {
            pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE,
        }]).1
    }
    for _ in 0..3 { render(&context, &mut panel, vec![]); }
    click(&context, &mut panel, "保持连接活跃（SSH 保活）");
    click(&context, &mut panel, "断线自动重连");
    let draft = panel.profile_draft.as_mut().unwrap();
    assert!(!draft.ssh_options.keepalive_enabled && !draft.ssh_options.auto_reconnect);
    draft.ssh_options.keepalive_interval_secs = 43;
    draft.ssh_options.keepalive_max = 4;
    draft.ssh_options.reconnect_delay_secs = 7;
    draft.ssh_options.reconnect_attempts = 9;
    draft.ssh_options.zmodem_download_directory = Some("D:/downloads/session".into());
    draft.ssh_options.sftp_verification=Some(cshell_domain::SftpVerification::Full);
    let expected = draft.ssh_options.clone();
    let command = click(&context, &mut panel, "保存配置").expect("save click");
    let ProfileClientCommand::Apply { changes, .. } = command else { panic!("expected non-secret settings save") };
    let connection = changes.into_iter().find_map(|change| match change.change {
        Some(profile_change::Change::UpsertSshConnection(data)) => Some(SshConnectionRecord::try_from(data).unwrap()),
        _ => None,
    }).unwrap();
    assert_eq!(connection.options, expected);
    panel.catalog.as_mut().unwrap().ssh_connections[0] = connection;
    let record = panel.profile_draft.as_ref().unwrap().record.clone();
    panel.select_profile(record);
    assert_eq!(panel.profile_draft.as_ref().unwrap().ssh_options, expected);
}

#[test]
fn new_local_profile_uses_home_and_existing_directory_policies_round_trip() {
    let id = ProfileId::new();
    let mut draft = LocalDraft::from_connection(None);
    draft.program = cshell_local::LocalProfile::platform_default().program.to_str().unwrap().into();
    let mut record = draft.record(id).unwrap();
    assert_eq!(record.cwd, LocalWorkingDirectory::Home);
    for cwd in [LocalWorkingDirectory::Inherit, LocalWorkingDirectory::Explicit {
        path: cshell_local::home_directory().unwrap().to_str().unwrap().into(),
    }] {
        record.cwd = cwd;
        assert_eq!(LocalDraft::from_connection(Some(&record)).record(id).unwrap(), record);
    }
}

fn panel() -> ProfilePanel {
    let mut panel = ProfilePanel::default();
    panel.catalog = Some(DesktopProfileCatalog {
        revision: 1,
        defaults: Default::default(),
        folders: vec![],
        profiles: vec![],
        ssh_connections: vec![],
        local_connections: vec![],
        themes: vec![],
    });
    panel.open_new_ssh_profile();
    let draft = panel.profile_draft.as_mut().unwrap();
    draft.record.name = "生产环境中文会话名称".repeat(8);
    draft.ssh_host = "long-hostname.example.test".into();
    draft.ssh_username = "operator".into();
    draft.private_key_path = format!("C:/Users/operator/{}/id_ed25519", "中文目录/".repeat(30));
    draft.certificate_path = format!("{}-cert.pub", draft.private_key_path);
    draft.agent_identity = "SHA256:abcdefghijklmnopqrstuvwxyz0123456789abcdef".into();
    let catalog = panel.catalog.as_mut().unwrap();
    let folder = ProfileFolder {
        id: FolderId::new(),
        name: "很长的中文文件夹名称".repeat(12),
        parent_id: None,
        terminal: Default::default(),
    };
    draft.record.folder_id = Some(folder.id);
    catalog.folders.push(folder);
    catalog.profiles.push(draft.record.clone());
    catalog.ssh_connections.push(SshConnectionRecord {
        options: Default::default(),
        profile_id: draft.record.id,
        host: draft.ssh_host.clone(),
        port: 22,
        username: draft.ssh_username.clone(),
        auth_method: SshAuthMethod::Password,
        private_key_path: None,
        certificate_path: None,
        agent_backend: SshAgentBackend::Auto,
        agent_identity: None,
        route: SshRoute::Direct,
    });
    let mut preview = HostKeyPreviewData::default();
    preview.profile_id = draft.record.id.as_uuid().as_bytes().to_vec();
    preview.host = draft.ssh_host.clone();
    preview.port = 22;
    preview.algorithm = "ssh-ed25519".into();
    preview.fingerprint = draft.agent_identity.clone();
    preview.public_key_line = format!("ssh-ed25519 {}", "abcdef".repeat(100));
    panel.host_key_preview = Some(preview);
    panel
}

#[test]
fn password_save_remove_clicks_are_blocked_while_pending_and_show_completion() {
    fn render(context: &egui::Context, panel: &mut ProfilePanel, events: Vec<egui::Event>)
        -> (egui::accesskit::TreeUpdate, Option<ProfileClientCommand>, Vec<String>)
    {
        let mut command = None;
        let mut output = context.run_ui(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1100.0, 780.0))),
            events, ..Default::default()
        }, |ui| { command = panel.draw(ui.ctx()); });
        output.textures_delta.clear();
        let text = output.shapes.iter().filter_map(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => Some(text.galley.job.text.clone()),
            _ => None,
        }).collect();
        (output.platform_output.accesskit_update.unwrap(), command, text)
    }
    fn click(pos: egui::Pos2) -> Vec<egui::Event> {
        vec![egui::Event::PointerMoved(pos),
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
            egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE }]
    }
    let context = context();
    let mut panel = panel();
    panel.editor_tab = 1;
    for _ in 0..3 { render(&context, &mut panel, vec![]); }
    for _ in 0..4 {
        let (tree, _, _) = render(&context, &mut panel, vec![]);
        let password = tree.nodes.iter().find(|(_, node)| node.label() == Some("密码输入")).unwrap();
        render(&context, &mut panel, click(bounds(&password.1).center()));
        render(&context, &mut panel, vec![egui::Event::Text("test-only-输入".into())]);
        assert_eq!(panel.profile_draft.as_ref().unwrap().password, "test-only-输入");
        for (button, saved) in [("保存密码", true), ("移除密码", false)] {
            let (tree, _, _) = render(&context, &mut panel, vec![]);
            let pos = bounds(&tree.nodes.iter().find(|(_, node)| node.label() == Some(button)).unwrap().1).center();
            let (_, command, _) = render(&context, &mut panel, click(pos));
            assert!(if saved { matches!(command, Some(ProfileClientCommand::SetPassword { .. })) }
                else { matches!(command, Some(ProfileClientCommand::DeletePassword { .. })) });
            assert!(panel.credential_pending);
            for _ in 0..20 { assert!(render(&context, &mut panel, click(pos)).1.is_none()); }
            let status = if saved { "密码已保存到凭据存储" } else { "密码已从凭据存储移除" };
            let view = DesktopProfileView {
                generation: panel.seen_generation + 1, catalog: panel.catalog.clone(), status: status.into(),
                ..Default::default()
            };
            panel.sync(&view);
            render(&context, &mut panel, vec![]);
            let (_, _, text) = render(&context, &mut panel, vec![]);
            assert!(text.iter().any(|text| text == status), "completion must be visibly painted");
        }
    }
}

#[test]
fn username_and_password_are_editable_on_the_same_authentication_page() {
    let context = context();
    let mut panel = panel();
    panel.editor_tab = 1;
    for _ in 0..3 { frame(&context, egui::vec2(900.0, 600.0), |ctx| { panel.draw(ctx); }); }
    let tree = frame(&context, egui::vec2(900.0, 600.0), |ctx| { panel.draw(ctx); });
    assert!(tree.nodes.iter().any(|(_, node)| node.value() == Some("operator")), "the account must be editable on this page");
    assert!(tree.nodes.iter().any(|(_, node)| node.label() == Some("密码输入")), "the password must be on this same page");
    check_window(&context, egui::vec2(900.0, 600.0), "连接管理", &tree);
}

#[test]
fn new_ssh_password_accepts_input_and_is_included_in_save() {
    let context = context();
    let mut panel = panel();
    let mut catalog = panel.catalog.take().unwrap();
    catalog.profiles.clear();
    catalog.ssh_connections.clear();
    let draft = panel.profile_draft.as_mut().unwrap();
    let mut draw = |events: Vec<egui::Event>| {
        let mut command = None;
        let mut output = context.run_ui(egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0))),
            events, ..Default::default()
        }, |ui| {
            draw_profile_page(ui, draft, &catalog, &[], None, 1);
            command = draw_profile_actions(ui, draft, &catalog).0;
        });
        output.textures_delta.clear();
        (output.platform_output.accesskit_update.unwrap(), command)
    };
    let pointer = |pos| vec![
        egui::Event::PointerMoved(pos),
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: true, modifiers: egui::Modifiers::NONE },
        egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed: false, modifiers: egui::Modifiers::NONE },
    ];
    let (tree, _) = draw(vec![]);
    let password = tree.nodes.iter().find(|(_, n)| n.label() == Some("密码输入")).unwrap().1.clone();
    assert!(!password.is_disabled(), "new SSH password field must be editable before saving");
    draw(pointer(bounds(&password).center()));
    let (tree, _) = draw(vec![egui::Event::Text("new-test-password".into())]);
    let save = tree.nodes.iter().find(|(_, n)| n.label() == Some("保存配置")).unwrap();
    let (_, command) = draw(pointer(bounds(&save.1).center()));
    let command = command.expect("save command");
    assert!(!format!("{command:?}").contains("new-test-password"));
    match command {
        ProfileClientCommand::ApplyWithPassword { profile_id, expected_revision, changes, password } => {
            assert_eq!(profile_id, panel.profile_draft.as_ref().unwrap().record.id);
            assert_eq!(expected_revision, catalog.revision);
            assert_eq!(&*password, b"new-test-password");
            assert!(!format!("{changes:?}").contains("new-test-password"));
        }
        _ => panic!("password must be saved with the new configuration"),
    }
    assert!(panel.profile_draft.as_ref().unwrap().password.is_empty());
}

#[test]
fn opening_a_connection_hides_the_editor_and_failures_preserve_connection_context() {
    let mut panel = panel();
    panel.profile_draft.as_mut().unwrap().password = "test-only-secret".into();
    panel.finish_open();
    assert!(!panel.open);
    assert!(panel.profile_draft.as_ref().unwrap().password.is_empty());
    assert!(panel.set_launch_error(Some("host key must be verified".into()), None));
    assert!(!panel.open,"connection errors do not force the settings editor open");
    assert!(panel.set_launch_error(Some("InvalidConfiguration: missing host".into()),None));
    assert!(!panel.open,"settings navigation is always explicit after a connection failure");
}

#[test]
fn unknown_host_key_automatically_previews_the_requested_profile_in_a_modal() {
    let context = context();
    let mut panel = panel();
    let requested = panel.profile_draft.as_ref().unwrap().record.id;
    panel.new_ssh_profile();
    panel.finish_open();
    assert!(panel.set_launch_error(Some("HostKeyUnknown: Unknown SSH host key".into()), Some(requested)));
    assert!(!panel.open, "the connection must not send users through the editor again");
    assert_eq!(panel.page, 0);
    assert_eq!(panel.editor_tab, 3);
    assert_eq!(panel.profile_draft.as_ref().unwrap().record.id, requested);
    let mut command = None;
    frame(&context, egui::vec2(900.0, 600.0), |ctx| command = panel.draw(ctx));
    assert!(matches!(command, Some(ProfileClientCommand::PreviewHostKey { profile_id, .. }) if profile_id == requested));
    for _ in 0..3 {
        frame(&context, egui::vec2(900.0, 600.0), |ctx| assert!(panel.draw(ctx).is_none()));
    }
    let tree = frame(&context, egui::vec2(900.0, 600.0), |ctx| assert!(panel.draw(ctx).is_none()));
    assert!(tree.nodes.iter().any(|(_, node)| node.label() == Some("取消")));
    assert!(!tree.nodes.iter().any(|(_, node)| node.label() == Some("确认并连接")), "no trust before a preview arrives");
}

#[test]
fn changed_key_waits_for_catalog_then_opens_update_prompt_once() {
    let context = context(); let mut panel = panel();
    let requested = panel.profile_draft.as_ref().unwrap().record.id;
    let catalog = panel.catalog.take().unwrap();
    let error = Some("HostKeyChanged: server reinstalled".into());
    panel.set_launch_error(error.clone(), Some(requested));
    assert!(panel.host_key_prompt.is_none());
    panel.sync(&DesktopProfileView { generation: 1, catalog: Some(catalog), ..Default::default() });
    let mut command = None;
    frame(&context, egui::vec2(900.0, 600.0), |ctx| command = panel.draw(ctx));
    assert!(matches!(command, Some(ProfileClientCommand::PreviewHostKeyUpdate { profile_id, .. }) if profile_id == requested));
    panel.host_key_prompt = None; // User dismissed the prompt.
    assert!(!panel.set_launch_error(error, Some(requested)));
    assert!(panel.host_key_prompt.is_none());
}

#[test]
fn identical_host_key_errors_from_different_profiles_open_the_correct_prompt() {
    let context = context(); let mut panel = panel();
    let first = panel.catalog.as_ref().unwrap().ssh_connections[0].clone();
    let mut second = first.clone(); second.profile_id = ProfileId::new(); second.host = "second.example.test".into();
    panel.catalog.as_mut().unwrap().ssh_connections.push(second.clone());
    let error = Some("HostKeyChanged: server reinstalled".into());
    panel.set_launch_error(error.clone(), Some(first.profile_id));
    panel.set_launch_error(error, Some(second.profile_id));
    let mut command = None;
    frame(&context, egui::vec2(900.0, 600.0), |ctx| command = panel.draw(ctx));
    assert!(matches!(command, Some(ProfileClientCommand::PreviewHostKeyUpdate { profile_id, .. }) if profile_id == second.profile_id));
}

#[test]
fn every_ssh_settings_page_keeps_save_and_connect_visible() {
    for size in [
        egui::vec2(1280.0, 800.0),
        egui::vec2(900.0, 600.0),
        egui::vec2(720.0, 480.0),
    ] {
        let context = context();
        let mut panel = panel();
        for tab in 0..6 {
            panel.editor_tab = tab;
            for mode in 0..4 {
                let draft = panel.profile_draft.as_mut().unwrap();
                draft.auth_method = [
                    SshAuthMethod::Password,
                    SshAuthMethod::PrivateKey,
                    SshAuthMethod::Certificate,
                    SshAuthMethod::Agent,
                ][mode];
                draft.ssh_options.authentication_order.clear();
                draft.ssh_options.keyboard_interactive = false;
                draft.route_kind = mode as u8;
                let mut draw = |c: &egui::Context| assert!(panel.draw(c).is_none());
                for _ in 0..2 {
                    frame(&context, size, &mut draw);
                }
                let tree = frame(&context, size, &mut draw);
                check_window(&context, size, "连接管理", &tree);
                let viewport = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                for label in ["保存配置", "连接已保存配置"] {
                    let node = tree
                        .nodes
                        .iter()
                        .find(|(_, n)| {
                            n.role() == egui::accesskit::Role::Button && n.label() == Some(label)
                        })
                        .unwrap_or_else(|| panic!("missing {label} on tab {tab}, mode {mode}"));
                    assert!(
                        viewport.contains_rect(bounds(&node.1)),
                        "footer {label} outside {size:?}, tab {tab}, mode {mode}"
                    );
                }
            }
        }
        panel.profile_draft.as_mut().unwrap().record.kind = ProfileKind::Local;
        panel.profile_draft.as_mut().unwrap().local.program = "C:/Windows/System32/cmd.exe".into();
        for tab in [0, 4] {
            panel.editor_tab = tab;
            for _ in 0..2 {
                frame(&context, size, |c| {
                    panel.draw(c);
                });
            }
            let tree = frame(&context, size, |c| {
                panel.draw(c);
            });
            check_window(&context, size, "连接管理", &tree);
        }
        panel.select_folder(panel.catalog.as_ref().unwrap().folders[0].clone());
        for _ in 0..2 {
            frame(&context, size, |c| {
                panel.draw(c);
            });
        }
        let tree = frame(&context, size, |c| {
            panel.draw(c);
        });
        check_window(&context, size, "连接管理", &tree);
        for page in [1, 2] {
            panel.page = page;
            for _ in 0..2 {
                frame(&context, size, |c| {
                    panel.draw(c);
                });
            }
            let tree = frame(&context, size, |c| {
                panel.draw(c);
            });
            check_window(&context, size, "连接管理", &tree);
        }
    }
}

#[test]
fn keyboard_interactive_option_saves_and_reloads_with_account_settings() {
    let context=context();let mut panel=panel();panel.editor_tab=1;panel.profile_draft.as_mut().unwrap().ssh_options.authentication_order=vec![cshell_domain::SshAuthStep::Password];panel.profile_draft.as_mut().unwrap().ssh_options.keyboard_interactive=false;
    fn render(context:&egui::Context,panel:&mut ProfilePanel,events:Vec<egui::Event>)->(egui::accesskit::TreeUpdate,Option<ProfileClientCommand>){
        let mut command=None;let mut output=context.run_ui(egui::RawInput {screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(1000.0,900.0))),events,..Default::default()},|ui|command=panel.draw(ui.ctx()));
        output.textures_delta.clear();(output.platform_output.accesskit_update.unwrap(),command)
    }
    fn click(context:&egui::Context,panel:&mut ProfilePanel,label:&str)->Option<ProfileClientCommand>{
        let (tree,_)=render(context,panel,vec![]);let pos=bounds(&tree.nodes.iter().find(|(_,n)|n.label()==Some(label)).unwrap().1).center();
        render(context,panel,vec![egui::Event::PointerMoved(pos),egui::Event::PointerButton {pos,button:egui::PointerButton::Primary,pressed:true,modifiers:egui::Modifiers::NONE}]);
        render(context,panel,vec![egui::Event::PointerButton {pos,button:egui::PointerButton::Primary,pressed:false,modifiers:egui::Modifiers::NONE}]).1
    }
    for _ in 0..3{render(&context,&mut panel,vec![]);}
    assert!(!panel.profile_draft.as_ref().unwrap().ssh_options.keyboard_interactive);
    click(&context,&mut panel,"调整认证方式与顺序");
    for _ in 0..3{render(&context,&mut panel,vec![]);}
    click(&context,&mut panel,"键盘交互验证（验证码 / 密码）");
    assert!(panel.profile_draft.as_ref().unwrap().ssh_options.keyboard_interactive);
    let Some(ProfileClientCommand::Apply {changes,..})=click(&context,&mut panel,"保存配置") else{panic!("save config")};
    let connection=changes.into_iter().find_map(|c|match c.change {Some(profile_change::Change::UpsertSshConnection(data))=>Some(SshConnectionRecord::try_from(data).unwrap()),_=>None}).unwrap();
    assert!(connection.options.keyboard_interactive);panel.catalog.as_mut().unwrap().ssh_connections[0]=connection;
    let record=panel.profile_draft.as_ref().unwrap().record.clone();panel.select_profile(record);assert!(panel.profile_draft.as_ref().unwrap().ssh_options.keyboard_interactive);
}

fn login_page_frame(context:&egui::Context,panel:&mut ProfilePanel,events:Vec<egui::Event>)->(egui::accesskit::TreeUpdate,Option<ProfileClientCommand>){
    context.all_styles_mut(|style|style.animation_time=0.0);
    let mut command=None;let mut output=context.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,egui::vec2(1000.0,900.0))),events,..Default::default()},|ui|command=panel.draw(ui.ctx()));output.textures_delta.clear();(output.platform_output.accesskit_update.unwrap(),command)
}
fn login_page_click(context:&egui::Context,panel:&mut ProfilePanel,label:&str)->Option<ProfileClientCommand>{
    let (tree,_)=login_page_frame(context,panel,vec![]);let node=&tree.nodes.iter().find(|(_,n)|n.label()==Some(label)&&!n.is_disabled()).unwrap_or_else(||panic!("missing enabled {label}: {:?}",tree.nodes.iter().map(|(_,n)|(n.role(),n.label(),n.value(),n.is_disabled())).collect::<Vec<_>>())).1;let pos=bounds(node).center();
    login_page_frame(context,panel,vec![egui::Event::PointerMoved(pos),egui::Event::PointerButton{pos,button:egui::PointerButton::Primary,pressed:true,modifiers:egui::Modifiers::NONE}]);
    login_page_frame(context,panel,vec![egui::Event::PointerButton{pos,button:egui::PointerButton::Primary,pressed:false,modifiers:egui::Modifiers::NONE}]).1
}
fn login_page_scroll(context:&egui::Context,panel:&mut ProfilePanel) {
    login_page_frame(context,panel,vec![egui::Event::PointerMoved(egui::pos2(700.0,400.0)),egui::Event::MouseWheel{phase:egui::TouchPhase::Move,unit:egui::MouseWheelUnit::Point,delta:egui::vec2(0.0,-400.0),modifiers:egui::Modifiers::NONE}]);
    for _ in 0..3 { login_page_frame(context,panel,vec![]); }
}
#[test]
fn login_first_page_shows_address_and_account_and_saves_with_empty_credentials(){
    let context=context();let mut panel=panel();panel.new_ssh_profile();panel.editor_tab=0;
    let draft=panel.profile_draft.as_mut().unwrap();draft.record.name="address only".into();draft.ssh_host="192.0.2.10".into();draft.ssh_port=2222;draft.ssh_username.clear();draft.password.clear();
    for _ in 0..3{login_page_frame(&context,&mut panel,vec![]);}
    let (tree,_)=login_page_frame(&context,&mut panel,vec![]);
    for label in ["主机","端口","用户名（可留空）","密码（可留空）","保存配置"]{assert!(tree.nodes.iter().any(|(_,n)|n.label()==Some(label)||n.value()==Some(label)),"{label}");}
    assert!(!tree.nodes.iter().any(|(_,n)|n.role()==egui::accesskit::Role::Button&&n.label()==Some("账号与认证")),"account fields share the connection page");
    let Some(ProfileClientCommand::Apply{changes,..})=login_page_click(&context,&mut panel,"保存配置") else{panic!("empty credentials must save without a secret mutation")};
    let connection=changes.into_iter().find_map(|c|match c.change{Some(profile_change::Change::UpsertSshConnection(data))=>Some(SshConnectionRecord::try_from(data).unwrap()),_=>None}).unwrap();
    assert_eq!(connection.host,"192.0.2.10");assert_eq!(connection.port,2222);assert!(connection.username.is_empty());assert_eq!(connection.options.authentication_order,vec![cshell_domain::SshAuthStep::KeyboardInteractive,cshell_domain::SshAuthStep::Password]);
}
#[test]
fn login_order_controls_reorder_and_save_the_method_sequence(){
    let context=context();let mut panel=panel();panel.editor_tab=0;
    panel.profile_draft.as_mut().unwrap().ssh_options.authentication_order=vec![cshell_domain::SshAuthStep::Password,cshell_domain::SshAuthStep::KeyboardInteractive];
    for _ in 0..3{login_page_frame(&context,&mut panel,vec![]);}
    login_page_scroll(&context,&mut panel);
    login_page_click(&context,&mut panel,"调整认证方式与顺序");for _ in 0..3{login_page_frame(&context,&mut panel,vec![]);}
    login_page_scroll(&context,&mut panel);
    login_page_click(&context,&mut panel,"↑");
    assert_eq!(panel.profile_draft.as_ref().unwrap().ssh_options.authentication_order[0],cshell_domain::SshAuthStep::KeyboardInteractive);
    let Some(ProfileClientCommand::Apply{changes,..})=login_page_click(&context,&mut panel,"保存配置") else{panic!("save order")};
    let connection=changes.into_iter().find_map(|c|match c.change{Some(profile_change::Change::UpsertSshConnection(data))=>Some(SshConnectionRecord::try_from(data).unwrap()),_=>None}).unwrap();assert_eq!(connection.options.authentication_order,vec![cshell_domain::SshAuthStep::KeyboardInteractive,cshell_domain::SshAuthStep::Password]);
}
