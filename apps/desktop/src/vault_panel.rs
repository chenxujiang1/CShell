use cshell_domain::ProfileId;
use cshell_ipc::{VaultOperation as Op, VaultRequest, VaultResponse};
use zeroize::{Zeroize, Zeroizing};

mod access;
mod protection;

pub struct VaultPanel {
    pub open: bool,
    pub response: Option<VaultResponse>,
    pub error: Option<String>,
    password: Zeroizing<String>,
    confirmation: Zeroizing<String>,
    passphrase: Zeroizing<String>,
    page: u8,
    keys: crate::managed_keys_panel::ManagedKeysPanel,
    busy: bool,
    notice: String,
    creation_system: bool,
    rebuilding: bool,
    rebuild_confirmed: bool,
    protection_target: Option<bool>,
}
impl Default for VaultPanel {
    fn default()->Self {Self{open:false,response:None,error:None,password:Zeroizing::new(String::new()),confirmation:Zeroizing::new(String::new()),passphrase:Zeroizing::new(String::new()),page:0,keys:Default::default(),busy:false,notice:String::new(),creation_system:true,rebuilding:false,rebuild_confirmed:false,protection_target:None}}
}
impl VaultPanel {
    pub fn owns_native_key_picker(&self) -> bool {
        self.open && self.keys.picker_open()
    }
    pub fn set_operation_state(&mut self, busy: bool, notice: String) {
        if self.busy && !busy && !notice.is_empty() && self.error.is_none() { self.keys.finished(); }
        if !busy && self.response.as_ref().is_some_and(|r| r.unlocked) { self.rebuilding = false; self.rebuild_confirmed = false; }
        self.busy = busy;
        self.notice = notice;
    }
    pub fn request_failed(&mut self) { self.busy = false; self.error = Some("请求未提交，请稍后重试".into()); }
    pub fn clear(&mut self) {
        self.password.zeroize();
        self.confirmation.zeroize();
        self.passphrase.zeroize();
        self.keys.clear();
        self.rebuild_confirmed = false;
    }
    pub fn draw(
        &mut self,
        context: &egui::Context,
        selected: Option<(ProfileId, u64)>,
    ) -> Option<VaultRequest> {
        if !self.open {
            return None;
        }
        let mut open = self.open;
        let mut outgoing = None;
        let response = self.response.clone();
        cshell_ui::form::dialog(&context, "凭据管理").open(&mut open).default_width(800.0).show(context, |ui| {
            ui.horizontal(|ui| {
                cshell_ui::form::tab(ui, &mut self.page, 0, "密钥对");
                cshell_ui::form::tab(ui, &mut self.page, 1, "连接凭据设置");
            });
            if let Some(error) = &self.error { ui.colored_label(cshell_ui::theme::DANGER, error); }
            if !self.notice.is_empty() { ui.label(&self.notice); }
            outgoing = self.draw_access(ui, selected);
            if self.busy || !response.as_ref().is_some_and(|r| r.unlocked) { return; }
            let unlocked = true;
            let available = response.as_ref().is_some_and(|r| r.status == cshell_ipc::VaultControlStatus::Ok as i32);
            if self.page == 0 {
                if outgoing.is_none() {
                    outgoing = self.keys.draw(ui, response.as_ref().map_or(&[], |r| r.managed_keys.as_slice()), available,
                        response.as_ref().map_or(0, |r| r.revision), response.as_ref().map_or(0, |r| r.lock_generation));
                }
                return;
            }
            let build = |operation| {
                let mut request = VaultRequest::new(operation).for_profile(selected.map_or_else(Vec::new, |(id, _)| id.as_uuid().as_bytes().to_vec()));
                request.expected_profile_revision = selected.map_or(0, |(_, revision)| revision);
                request.expected_vault_revision = response.as_ref().map_or(0, |r| r.revision);
                request.expected_lock_generation = response.as_ref().map_or(0, |r| r.lock_generation);
                request
            };
            ui.separator();
            if let Some((id, _)) = selected {
                let current = response.as_ref().filter(|r| r.profile_id == id.as_uuid().as_bytes());
                let ready = unlocked && available && current.is_some();
                ui.label("当前已保存 SSH Profile 的凭据存储");
                ui.label("切换模式不复制已有凭据。选择 Vault 后，缺失凭据不会回退到钥匙串或原私钥文件。");
                if let Some(current) = current.filter(|r| r.unlocked) {
                    ui.label(if current.portable { "模式：加密 Vault" } else { "模式：系统钥匙串 / 私钥文件" });
                    ui.label(format!("Vault 记录：密码 {} / 私钥 {} / 私钥口令 {}", current.has_password, current.has_private_key, current.has_key_passphrase));
                }
                ui.horizontal_wrapped(|ui| {
                    for (label, portable) in [("使用系统存储", false), ("使用加密 Vault", true)] {
                        if ui.add_enabled(ready, egui::Button::new(label)).clicked() { let mut r = build(Op::SetMode); r.portable = portable; outgoing = Some(r); }
                    }
                });
                ui.label("从已保存的私钥路径验证并加密导入。源文件不会被删除；私钥字节不经过 GUI。");
                ui.label("支持现有密钥格式及有界 PPK v2/v3（RSA 2048～4096 位、Ed25519、NIST ECDSA）：检查文件 MAC 和公私钥配对，PPK v3 KDF 超出资源上限会拒绝。RSA 仅使用 SHA-2 签名；DSA/Ed448 PPK 当前拒绝，PPK 注释/私钥内容不显示。\nPPK 解密口令与 Vault 主密码独立；密码不从外部会话文件提取。源文件原样保留，导入不连接服务器。");
                ui.label("导入口令（未加密私钥留空）");
                ui.add_enabled_ui(ready, |ui| { crate::secret_edit::edit(ui, &mut self.passphrase, "vault-import-phrase", 4096); });
                let portable = ready && current.is_some_and(|r| r.portable);
                ui.horizontal_wrapped(|ui| {
                    if ui.add_enabled(portable, egui::Button::new("验证并导入私钥")).clicked() {
                        let mut r = build(Op::ImportPrivateKey); r.secret = std::mem::take(&mut *self.passphrase).into_bytes(); outgoing = Some(r);
                    }
                    if ui.add_enabled(portable && current.is_some_and(|r| r.has_private_key), egui::Button::new("移除导入私钥")).clicked() { outgoing = Some(build(Op::DeletePrivateKey)); }
                });
            } else { ui.label("在连接管理中选择已保存的 SSH Profile，以设置模式或导入私钥。"); }
        });
        if !open {
            self.clear();
            self.rebuilding = false;
        }
        self.open = open;
        if outgoing.as_ref().is_some_and(|r| r.operation != Op::Status as i32) { self.busy = true; self.notice.clear(); }
        outgoing
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    fn draw(
        panel: &mut VaultPanel,
        context: &egui::Context,
        selected: Option<(ProfileId, u64)>,
        events: Vec<egui::Event>,
    ) -> (egui::accesskit::TreeUpdate, Option<VaultRequest>) {
        let mut request = None;
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 1000.0),
                )),
                events,
                ..Default::default()
            },
            |ui| request = panel.draw(ui.ctx(), selected),
        );
        output.textures_delta.clear();
        (output.platform_output.accesskit_update.unwrap(), request)
    }
    fn click(update: &egui::accesskit::TreeUpdate, label: &str) -> Vec<egui::Event> {
        let bounds = update
            .nodes
            .iter()
            .find(|(_, node)| {
                matches!(node.role(), egui::accesskit::Role::Button | egui::accesskit::Role::CheckBox) && node.label() == Some(label)
            })
            .unwrap()
            .1
            .bounds()
            .unwrap();
        let pos = egui::pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
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
        ]
    }
    #[test]
    fn unlocked_credentials_remain_editable_for_locked_suspended_and_unavailable_desktops() {
        for state in [cshell_ipc::VaultSessionState::Locked, cshell_ipc::VaultSessionState::Suspended,
            cshell_ipc::VaultSessionState::Unavailable] {
            let context = egui::Context::default();
            context.enable_accesskit();
            let id = ProfileId::new();
            let mut panel = VaultPanel {
                open: true, page: 1,
                response: Some(VaultResponse {
                    exists: true, unlocked: true, session_state: state as i32,
                    status: cshell_ipc::VaultControlStatus::Ok as i32,
                    profile_id: id.as_uuid().as_bytes().to_vec(), ..Default::default()
                }), ..Default::default()
            };
            draw(&mut panel, &context, Some((id, 1)), vec![]);
            let (tree, _) = draw(&mut panel, &context, Some((id, 1)), vec![]);
            let (_, request) = draw(&mut panel, &context, Some((id, 1)), click(&tree, "使用加密 Vault"));
            assert_eq!(request.unwrap().operation, Op::SetMode as i32);
            panel.busy = false;
            let (tree, _) = draw(&mut panel, &context, Some((id, 1)), vec![]);
            let (_, request) = draw(&mut panel, &context, Some((id, 1)), click(&tree, "立即锁定"));
            assert_eq!(request.unwrap().operation, Op::Lock as i32);
        }
    }

    #[test]
    fn keys_page_unlocks_in_place_and_immediately_shows_key_actions() {
        let context = egui::Context::default(); context.enable_accesskit();
        let mut panel = VaultPanel { open: true, response: Some(VaultResponse { exists: true, revision: 9, lock_generation: 17, ..Default::default() }), password: Zeroizing::new("test-password".into()), ..Default::default() };
        draw(&mut panel,&context,None,vec![]); let (tree,_) = draw(&mut panel,&context,None,vec![]);
        let (_,request) = draw(&mut panel,&context,None,click(&tree,"解锁"));
        let request=request.unwrap(); assert_eq!(request.operation,Op::Unlock as i32); assert_eq!(request.secret,b"test-password"); assert_eq!(panel.page,0); assert!(panel.password.is_empty());
        panel.response.as_mut().unwrap().unlocked=true; panel.set_operation_state(false,String::new());
        let (tree,_) = draw(&mut panel,&context,None,vec![]);
        assert!(tree.nodes.iter().any(|(_,n)|n.label()==Some("生成新密钥对…")));
    }
    #[test]
    fn keys_page_creates_or_unlocks_both_protection_modes_without_switching_tabs() {
        for (exists,system,label,operation) in [(false,false,"创建并解锁凭据库",Op::Create),(false,true,"创建系统钥匙串保护的凭据库",Op::CreateSystem),(true,true,"通过系统钥匙串解锁",Op::UnlockSystem)] {
            let context=egui::Context::default();context.enable_accesskit();
            let mut panel=VaultPanel { open:true, creation_system:system, response:Some(VaultResponse{exists,system,..Default::default()}), password:Zeroizing::new("test-password".into()),confirmation:Zeroizing::new("test-password".into()),..Default::default() };
            draw(&mut panel,&context,None,vec![]);let (tree,_)=draw(&mut panel,&context,None,vec![]);
            let (_,request)=draw(&mut panel,&context,None,click(&tree,label));let request=request.unwrap();assert_eq!(request.operation,operation as i32);assert_eq!(panel.page,0);
            if system {assert!(request.secret.is_empty());} else {assert_eq!(request.secret,b"test-password");}
        }
    }
    #[test]
    fn rebuild_requires_explicit_acknowledgement_and_sends_global_fenced_request() {
        let context=egui::Context::default();context.enable_accesskit();
        let mut panel=VaultPanel { open:true,rebuilding:true,creation_system:false,response:Some(VaultResponse{exists:true,system:true,revision:9,lock_generation:17,..Default::default()}),password:Zeroizing::new("new-test-password".into()),confirmation:Zeroizing::new("new-test-password".into()),..Default::default() };
        let selected=Some((ProfileId::new(),55));draw(&mut panel,&context,selected,vec![]);let(tree,_)=draw(&mut panel,&context,selected,vec![]);
        assert!(draw(&mut panel,&context,selected,click(&tree,"保留旧库并重建")).1.is_none());
        let(tree,_)=draw(&mut panel,&context,selected,vec![]);
        draw(&mut panel,&context,selected,click(&tree,"我理解新库为空，需要重新添加凭据，旧库不会自动恢复"));
        let(tree,_)=draw(&mut panel,&context,selected,vec![]);
        let request=draw(&mut panel,&context,selected,click(&tree,"保留旧库并重建")).1.unwrap();
        assert_eq!(request.operation,Op::RebuildPassword as i32);assert!(request.confirm_rebuild);assert!(request.profile_id.is_empty());assert_eq!(request.expected_profile_revision,0);
        assert_eq!(request.expected_vault_revision,9);assert_eq!(request.expected_lock_generation,17);assert_eq!(request.secret,b"new-test-password");assert!(panel.password.is_empty() && panel.confirmation.is_empty());
    }
    #[test]
    fn cancelling_rebuild_clears_confirmation_and_secrets_without_a_request() {
        let context=egui::Context::default();context.enable_accesskit();
        let mut panel=VaultPanel{open:true,rebuilding:true,rebuild_confirmed:true,response:Some(VaultResponse{exists:true,..Default::default()}),password:Zeroizing::new("test-secret".into()),..Default::default()};
        draw(&mut panel,&context,None,vec![]);let(tree,_)=draw(&mut panel,&context,None,vec![]);
        assert!(draw(&mut panel,&context,None,click(&tree,"取消重建")).1.is_none());assert!(!panel.rebuilding && !panel.rebuild_confirmed && panel.password.is_empty());
    }
    #[test]
    fn inline_create_unlock_and_rebuild_fit_small_workbench_windows() {
        for size in [egui::vec2(900.0,600.0),egui::vec2(720.0,480.0)] {
            for (exists,system,rebuilding) in [(false,false,false),(false,true,false),(true,false,false),(true,true,false),(true,false,true),(true,true,true)] {
                let context=crate::ui_layout_gate::context();let mut panel=VaultPanel{open:true,creation_system:system,rebuilding,response:Some(VaultResponse{exists,system,..Default::default()}),..Default::default()};
                let mut draw=|ctx:&egui::Context|{let _=panel.draw(ctx,None);};
                for _ in 0..2 {crate::ui_layout_gate::frame(&context,size,&mut draw);}
                let tree=crate::ui_layout_gate::frame(&context,size,&mut draw);crate::ui_layout_gate::check_window(&context,size,"凭据管理",&tree);
            }
        }
    }
    #[test]
    fn unlock_consumes_transient_password_and_carries_lock_generation() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let id = ProfileId::new();
        let mut panel = VaultPanel {
            open: true,
            page: 1,
            response: Some(VaultResponse {
                exists: true,
                revision: 19,
                lock_generation: 42,
                ..Default::default()
            }),
            password: Zeroizing::new("test-secret".into()),
            ..Default::default()
        };
        draw(&mut panel, &context, Some((id, 71)), vec![]);
        let (update, _) = draw(&mut panel, &context, Some((id, 71)), vec![]);
        let (_, request) = draw(&mut panel, &context, Some((id, 71)), click(&update, "解锁"));
        let request = request.unwrap();
        assert_eq!(request.operation, Op::Unlock as i32);
        assert_eq!(request.secret, b"test-secret");
        assert_eq!(request.expected_lock_generation, 42);
        assert_eq!(request.expected_vault_revision, 19);
        assert_eq!(request.expected_profile_revision, 71);
        assert!(panel.password.is_empty());
        assert!(panel.confirmation.is_empty());
        assert!(
            draw(&mut panel, &context, Some((id, 71)), vec![])
                .1
                .is_none()
        );
    }
    #[test]
    fn credential_mode_buttons_require_metadata_for_the_selected_saved_profile() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let id = ProfileId::new();
        let mut panel = VaultPanel {
            open: true,
            page: 1,
            response: Some(VaultResponse {
                exists: true,
                unlocked: true,
                system: true,
                revision: 19,
                profile_id: ProfileId::new().as_uuid().as_bytes().to_vec(),
                ..Default::default()
            }),
            ..Default::default()
        };
        draw(&mut panel, &context, Some((id, 71)), vec![]);
        let (update, _) = draw(&mut panel, &context, Some((id, 71)), vec![]);
        assert!(
            draw(
                &mut panel,
                &context,
                Some((id, 71)),
                click(&update, "使用加密 Vault")
            )
            .1
            .is_none()
        );
        panel.response.as_mut().unwrap().profile_id = id.as_uuid().as_bytes().to_vec();
        let (update, _) = draw(&mut panel, &context, Some((id, 71)), vec![]);
        let (_, request) = draw(
            &mut panel,
            &context,
            Some((id, 71)),
            click(&update, "使用加密 Vault"),
        );
        let request = request.unwrap();
        assert_eq!(request.operation, Op::SetMode as i32);
        assert!(request.portable);
        assert_eq!(request.expected_vault_revision, 19);
        assert_eq!(request.expected_profile_revision, 71);
        assert!(request.secret.is_empty());
    }
}
