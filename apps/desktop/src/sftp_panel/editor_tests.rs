use super::*;
use std::sync::Arc;

fn fixture() -> (AdvancedSftp, SftpView, SessionId) {
    let session = SessionId::new();
    let content = sftp_digest(b"original\n");
    let edit = SftpEditDocument {
        native_transfer: None,
        base_permissions: Some(0o644),
        peer: cshell_domain::ClipboardTrust {
            host: cshell_domain::ClipboardHost::new("host.example", 22).unwrap(),
            username: "operator".into(),
            host_key_sha256: "SHA256:test".into(),
            route: String::new(),
        },
        state: SftpEditState::Ready,
        queued_draft: None,
        format: "cshell.sftp-edit".into(),
        version: 1,
        id: SessionId::new(),
        session,
        revision: 2,
        remote_path: "/etc/example.conf".into(),
        local_path: std::env::temp_dir()
            .join("cshell-edit-fixture")
            .to_string_lossy()
            .into(),
        base: content,
        current: content,
        queued_item: None,
        closed: false,
        detail: "Private diagnostic details".into(),
    };
    let view = SftpView {
        session_id: Some(session),
        edit: Some(Arc::new(edit)),
        edit_text: Some(Arc::new("original\n".into())),
        edit_writable: true,
        queue_writable: true,
        queue: Some(Arc::new(SftpQueueSnapshot::initial(session))),
        ..Default::default()
    };
    (AdvancedSftp::default(), view, session)
}
fn frame(
    ctx: &egui::Context,
    panel: &mut AdvancedSftp,
    view: &SftpView,
    events: Vec<egui::Event>,
) -> (egui::accesskit::TreeUpdate, Option<SftpClientCommand>) {
    let mut command = None;
    let mut out = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 480.0),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                panel.editor(
                    ui,
                    view.session_id.unwrap(),
                    view,
                    "/etc/example.conf",
                    &mut command,
                )
            });
        },
    );
    out.textures_delta.clear();
    (out.platform_output.accesskit_update.unwrap(), command)
}
fn save() -> Vec<egui::Event> {
    vec![
        egui::Event::ModifiersChanged(egui::Modifiers::CTRL),
        egui::Event::Key {
            key: egui::Key::S,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::CTRL,
        },
    ]
}
#[test]
fn control_s_saves_once_without_a_checkbox_and_keeps_strict_publication() {
    let ctx = crate::ui_layout_gate::context();
    let (mut panel, view, _) = fixture();
    let (tree, _) = frame(&ctx, &mut panel, &view, vec![]);
    assert!(
        tree.nodes
            .iter()
            .any(|(_, n)| n.label() == Some("保存并上传    Ctrl+S"))
    );
    assert!(
        !tree
            .nodes
            .iter()
            .any(|(_, n)| n.label() == Some("Private diagnostic details"))
    );
    panel.edit_buffer = "changed\n".into();
    panel.edit_dirty = true;
    let (_, command) = frame(&ctx, &mut panel, &view, save());
    let Some(SftpClientCommand::Workspace(session, command)) = command else {
        panic!("save action missing");
    };
    assert_eq!(Some(session), view.session_id);
    assert_eq!(command.operation, SftpOperation::EditSave);
    let data: SftpEditSave = serde_json::from_slice(&command.data).unwrap();
    assert!(data.accept_exclusive_replace);
    assert_eq!(data.content.as_deref(), Some("changed\n"));
    assert_eq!(data.queue_revision, view.queue.as_ref().unwrap().revision);
    assert!(frame(&ctx, &mut panel, &view, save()).1.is_none());
    assert!(panel.edit_dirty);
    panel.editor_request_rejected();
    assert!(frame(&ctx, &mut panel, &view, save()).1.is_some());
}
#[test]
fn stale_revision_disconnected_or_busy_editor_never_sends_saved_text() {
    for case in 0..4 {
        let ctx = crate::ui_layout_gate::context();
        let (mut panel, mut view, _) = fixture();
        frame(&ctx, &mut panel, &view, vec![]);
        panel.edit_dirty = true;
        panel.edit_buffer = "keep my draft".into();
        match case {
            0 => view.edit_writable = false,
            1 => view.queue_busy = true,
            2 => {
                let mut edit = (**view.edit.as_ref().unwrap()).clone();
                edit.current = sftp_digest(b"external");
                edit.revision += 1;
                view.edit = Some(Arc::new(edit));
            }
            _ => view.workspace_busy = true,
        }
        assert!(frame(&ctx, &mut panel, &view, save()).1.is_none());
        assert_eq!(panel.edit_buffer, "keep my draft");
        assert!(panel.edit_dirty);
    }
}
#[test]
fn opening_another_file_never_overwrites_unsaved_draft() {
    let (mut panel, _, session) = fixture();
    panel.edit_dirty = true;
    panel.edit_buffer = "keep".into();
    assert!(panel.prepare_editor("/other/file",session));
    assert_eq!(panel.edit_buffer, "keep");
    assert_eq!(panel.editor_target.as_deref(),Some("/other/file"));
}
#[test]
fn document_switch_restores_unsaved_text_and_never_sends_it_to_another_file(){
    let ctx=crate::ui_layout_gate::context();let (mut panel,view,_)=fixture();frame(&ctx,&mut panel,&view,vec![]);
    panel.edit_buffer="unsaved first document".into();panel.edit_dirty=true;
    assert!(panel.prepare_editor("/etc/second.conf",view.session_id.unwrap()));
    let mut second=view.clone();let mut doc=(**view.edit.as_ref().unwrap()).clone();doc.id=SessionId::new();doc.remote_path="/etc/second.conf".into();second.edit=Some(Arc::new(doc));second.edit_text=Some(Arc::new("second document".into()));
    assert!(frame(&ctx,&mut panel,&second,vec![]).1.is_none());assert_eq!(panel.edit_buffer,"second document");assert!(!panel.edit_dirty);
    assert!(frame(&ctx,&mut panel,&view,vec![]).1.is_none());assert_eq!(panel.edit_buffer,"unsaved first document");assert!(panel.edit_dirty);
}
#[test]
fn long_file_editor_keeps_save_in_fixed_footer_even_with_find_bar(){
    let ctx=crate::ui_layout_gate::context();let (advanced,mut view,session)=fixture();view.edit_text=Some(Arc::new("line content\n".repeat(2000)));
    let mut panel=SftpPanel::default();panel.bound_session_id=Some(session);panel.page=2;panel.advanced=advanced;
    for frame in 0..4 {
        let events=if frame==2{vec![egui::Event::ModifiersChanged(egui::Modifiers::CTRL),egui::Event::Key{key:egui::Key::F,physical_key:None,pressed:true,repeat:false,modifiers:egui::Modifiers::CTRL}]}else{vec![]};
        let size=egui::vec2(720.0,480.0);let mut out=ctx.run_ui(egui::RawInput{screen_rect:Some(egui::Rect::from_min_size(egui::Pos2::ZERO,size)),events,..Default::default()},|ui|{let _=panel.draw_contents(ui,Some(session),Some(&view));});out.textures_delta.clear();
        let tree=out.platform_output.accesskit_update.unwrap();let save=&tree.nodes.iter().find(|(_,n)|n.label()==Some("保存并上传    Ctrl+S")).unwrap().1;
        assert!(egui::Rect::from_min_size(egui::Pos2::ZERO,size).contains_rect(crate::ui_layout_gate::bounds(save)));assert!(crate::ui_layout_gate::bounds(save).top()>300.0);
    }
}

#[test]
fn review_is_read_only_and_rebase_reply_preserves_unsaved_draft(){
    let ctx=crate::ui_layout_gate::context();let(mut panel,mut view,session)=fixture();
    frame(&ctx,&mut panel,&view,vec![]);
    panel.edit_buffer="my unsaved merge\n".into();panel.edit_dirty=true;
    let doc=view.edit.as_ref().unwrap().clone();let remote=sftp_digest(b"server changed\n");
    view.edit_review=Some(Arc::new(SftpEditReview{session,id:doc.id,revision:doc.revision,base_content:doc.base,base:Some("original\n".into()),remote_content:remote,remote_permissions:Some(0o644),remote:"server changed\n".into(),backups:vec![],selected_backup:None,backup_text:None}));
    panel.preview_editor_review(doc.id);
    assert!(frame(&ctx,&mut panel,&view,vec![]).1.is_none());assert_eq!(panel.edit_buffer,"my unsaved merge\n");
    panel.review.pending=Some((doc.id,doc.revision,remote));
    let mut updated=(*doc).clone();updated.revision+=1;updated.base=remote;view.edit=Some(Arc::new(updated));
    assert!(frame(&ctx,&mut panel,&view,vec![]).1.is_none());
    assert_eq!(panel.edit_buffer,"my unsaved merge\n");assert!(panel.edit_dirty);assert_eq!(panel.edit_stamp.unwrap().3,remote);
}
