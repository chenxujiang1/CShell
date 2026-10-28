use super::*;
fn challenge() -> KeyboardAuthChallenge {
    KeyboardAuthChallenge {
        id: vec![3; 16],
        target: "operator@mfa.test:22".into(),
        name: "二次验证".into(),
        instructions: "请填写一次性验证码".into(),
        prompts: vec![cshell_ipc::KeyboardAuthPrompt {
            text: "Verification code:".into(),
            echo: false,
        }],
        ..Default::default()
    }
}
fn frame(
    context: &egui::Context,
    dialog: &mut Dialog,
    events: Vec<egui::Event>,
) -> egui::FullOutput {
    context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 480.0),
            )),
            events,
            ..Default::default()
        },
        |ui| dialog.draw(ui.ctx()),
    )
}
fn key(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}
#[test]
fn verification_modal_accepts_multiple_characters_and_enter_submits_once_without_accessibility_secret()
 {
    let context = egui::Context::default();
    crate::ui_fonts::install(&context);
    context.enable_accesskit();
    let view = View {
        challenge: Some(challenge()),
        generation: 1,
        ..Default::default()
    };
    let shared = Arc::new(Mutex::new(view.clone()));
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let (stop, _) = tokio::sync::watch::channel(false);
    let mut dialog = Dialog {
        connection: Some(Connection {
            shared,
            sender,
            stop,
            worker: None,
        }),
        view,
        answers: vec![String::new()],
        focus: true,
        username: String::new(),
        save_credentials: false,
        last_catalog_refresh: 0,
    };
    for _ in 0..3 {
        let mut output = frame(&context, &mut dialog, vec![]);
        output.textures_delta.clear();
    }
    let mut output = frame(
        &context,
        &mut dialog,
        vec![
            egui::Event::Text("654".into()),
            egui::Event::Text("321".into()),
        ],
    );
    assert_eq!(dialog.answers[0], "654321");
    assert!(!format!("{:?}", output.platform_output.accesskit_update).contains("654321"));
    output.textures_delta.clear();
    let mut output = frame(&context, &mut dialog, vec![key(egui::Key::Enter)]);
    output.textures_delta.clear();
    let request = receiver.try_recv().unwrap();
    let answer = request.auth_answer.as_ref().unwrap();
    assert_eq!(answer.answers, vec!["654321"]);
    assert!(!answer.cancel);
    assert!(dialog.answers[0].is_empty());
    assert!(dialog.view.busy);
    let mut output = frame(&context, &mut dialog, vec![key(egui::Key::Enter)]);
    output.textures_delta.clear();
    assert!(receiver.try_recv().is_err());
    assert!(dialog.blocks_input());
}
#[test]
fn escape_cancels_without_submitting_typed_secrets() {
    let context = egui::Context::default();
    let view = View {
        challenge: Some(challenge()),
        generation: 1,
        ..Default::default()
    };
    let shared = Arc::new(Mutex::new(view.clone()));
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let (stop, _) = tokio::sync::watch::channel(false);
    let mut dialog = Dialog {
        connection: Some(Connection {
            shared,
            sender,
            stop,
            worker: None,
        }),
        view,
        answers: vec!["typed-otp".into()],
        focus: true,
        username: String::new(),
        save_credentials: false,
        last_catalog_refresh: 0,
    };
    for _ in 0..2 {
        let mut output = frame(&context, &mut dialog, vec![]);
        output.textures_delta.clear();
    }
    let mut output = frame(&context, &mut dialog, vec![key(egui::Key::Escape)]);
    output.textures_delta.clear();
    let request = receiver.try_recv().unwrap();
    let answer = request.auth_answer.as_ref().unwrap();
    assert!(answer.cancel);
    assert!(answer.answers.is_empty());
    assert!(dialog.answers[0].is_empty());
}
#[test]
fn cancel_button_works_with_empty_login_or_otp_and_never_submits_save_or_credentials_twice() {
    for login in [false,true] {
        let context=egui::Context::default();context.enable_accesskit();crate::ui_fonts::install(&context);
        let view=View{challenge:Some(KeyboardAuthChallenge{login,username_editable:login,..challenge()}),generation:1,..Default::default()};
        let shared=Arc::new(Mutex::new(view.clone()));let (sender,mut receiver)=tokio::sync::mpsc::channel(1);
        let mut dialog=Dialog{connection:Some(Connection{shared,sender,stop:tokio::sync::watch::channel(false).0,worker:None}),view,answers:vec![String::new()],username:String::new(),save_credentials:true,last_catalog_refresh:0,focus:true};
        let mut tree=None;
        for _ in 0..3 {let mut output=frame(&context,&mut dialog,vec![]);tree=output.platform_output.accesskit_update.take();output.textures_delta.clear();}
        let tree=tree.unwrap();let node=&tree.nodes.iter().find(|(_,node)|node.label()==Some("取消连接")).unwrap().1;
        let position=crate::ui_layout_gate::bounds(node).center();
        for pressed in [true,false] {let mut output=frame(&context,&mut dialog,vec![egui::Event::PointerMoved(position),egui::Event::PointerButton{pos:position,button:egui::PointerButton::Primary,pressed,modifiers:egui::Modifiers::NONE}]);output.textures_delta.clear();}
        let request=receiver.try_recv().unwrap();let answer=request.auth_answer.as_ref().unwrap();assert!(answer.cancel);assert!(answer.answers.is_empty());assert!(answer.username.is_empty());assert!(!answer.save_credentials);
        let mut output=frame(&context,&mut dialog,vec![key(egui::Key::Escape)]);output.textures_delta.clear();assert!(receiver.try_recv().is_err());
    }
}

#[test]
fn login_dialog_submits_username_password_and_explicit_save_choice() {
    let context = egui::Context::default();
    crate::ui_fonts::install(&context);
    context.enable_accesskit();
    let challenge = KeyboardAuthChallenge {
        login: true,
        username_editable: true,
        ..challenge()
    };
    let view = View {
        challenge: Some(challenge),
        generation: 1,
        ..Default::default()
    };
    let shared = Arc::new(Mutex::new(view.clone()));
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let (stop, _) = tokio::sync::watch::channel(false);
    let mut dialog = Dialog {
        connection: Some(Connection {
            shared,
            sender,
            stop,
            worker: None,
        }),
        view,
        answers: vec![String::new()],
        username: String::new(),
        save_credentials: false,
        last_catalog_refresh: 0,
        focus: true,
    };
    for _ in 0..3 {
        let mut output = frame(&context, &mut dialog, vec![]);
        output.textures_delta.clear();
    }
    let mut output = frame(
        &context,
        &mut dialog,
        vec![egui::Event::Text("operator".into())],
    );
    output.textures_delta.clear();
    assert_eq!(dialog.username, "operator");
    fn click(context: &egui::Context, dialog: &mut Dialog, label: &str) {
        let mut output = frame(context, dialog, vec![]);
        let tree = output.platform_output.accesskit_update.take().unwrap();
        output.textures_delta.clear();
        let node = &tree
            .nodes
            .iter()
            .find(|(_, n)| n.label() == Some(label))
            .unwrap()
            .1;
        let b = node.bounds().unwrap();
        let pos = egui::pos2(((b.x0 + b.x1) * 0.5) as f32, ((b.y0 + b.y1) * 0.5) as f32);
        let mut output = frame(
            context,
            dialog,
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        output.textures_delta.clear();
        let mut output = frame(
            context,
            dialog,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        output.textures_delta.clear();
    }
    click(&context, &mut dialog, "密码输入");
    let mut output = frame(
        &context,
        &mut dialog,
        vec![egui::Event::Text("correct-password".into())],
    );
    assert!(!format!("{:?}", output.platform_output.accesskit_update).contains("correct-password"));
    output.textures_delta.clear();
    click(&context, &mut dialog, "保存账号与密码（认证成功后保存）");
    assert!(dialog.save_credentials);
    click(&context, &mut dialog, "继续验证");
    let request = receiver.try_recv().unwrap();
    let answer = request.auth_answer.as_ref().unwrap();
    assert_eq!(answer.username, "operator");
    assert_eq!(answer.answers, vec!["correct-password"]);
    assert!(answer.save_credentials);
    assert!(dialog.answers[0].is_empty());
}

#[test]
fn vault_unlock_is_a_local_prompt_without_server_password_saving_and_clears_after_submit() {
    let context=egui::Context::default();crate::ui_fonts::install(&context);context.enable_accesskit();let mut challenge=challenge();challenge.vault_unlock=true;challenge.name="解锁凭据库".into();challenge.prompts[0].text="凭据库主密码".into();let view=View{challenge:Some(challenge),generation:1,..Default::default()};let shared=Arc::new(Mutex::new(view.clone()));let(sender,mut receiver)=tokio::sync::mpsc::channel(1);let(stop,_)=tokio::sync::watch::channel(false);let mut dialog=Dialog{connection:Some(Connection{shared,sender,stop,worker:None}),view,answers:vec![String::new()],focus:true,username:String::new(),save_credentials:false,last_catalog_refresh:0};
    let mut rendered_labels=String::new();
    for _ in 0..3{let mut output=frame(&context,&mut dialog,vec![]);output.textures_delta.clear();if let Some(tree)=output.platform_output.accesskit_update {for (_,node) in tree.nodes {if let Some(text)=node.label().or(node.value()){rendered_labels.push_str(text);rendered_labels.push(' ');}}}}let mut output=frame(&context,&mut dialog,vec![egui::Event::Text("local-master-secret".into())]);output.textures_delta.clear();let tree=output.platform_output.accesskit_update.unwrap();let labels=rendered_labels+&tree.nodes.iter().filter_map(|(_,n)|n.label().or(n.value())).collect::<Vec<_>>().join(" ");assert!(labels.contains("解锁并连接"));assert!(labels.contains("本地凭据库"));assert!(!labels.contains("保存账号与密码"));assert!(!format!("{tree:?}").contains("local-master-secret"));
    let mut output=frame(&context,&mut dialog,vec![key(egui::Key::Enter)]);output.textures_delta.clear();let request=receiver.try_recv().unwrap();assert_eq!(request.auth_answer.as_ref().unwrap().answers,vec!["local-master-secret"]);assert!(dialog.answers.iter().all(String::is_empty));assert!(!request.auth_answer.as_ref().unwrap().save_credentials);
}
