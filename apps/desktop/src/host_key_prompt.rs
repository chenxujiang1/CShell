use crate::profile_connection::{DesktopProfileView, ProfileClientCommand};
use cshell_domain::{ProfileId, SshConnectionRecord};
use cshell_ipc::HostKeyPreviewData;
use zeroize::Zeroizing;

enum Stage {
    Start,
    Scanning,
    Ready(HostKeyPreviewData),
    ExistingEntry,
    Confirming,
    Complete,
    Failed(String),
}
impl HostKeyPrompt {
    pub(crate) fn cancelled_connection_profile(&self)->Option<ProfileId> {
        self.reconnect.then_some(self.target.profile_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile_connection::DesktopProfileCatalog;

    fn fixture() -> (HostKeyPrompt, DesktopProfileView) {
        let target = SshConnectionRecord {
            options: Default::default(),
            profile_id: ProfileId::new(),
            host: "server.example.test".into(),
            port: 22,
            username: "operator".into(),
            auth_method: cshell_domain::SshAuthMethod::Password,
            private_key_path: None,
            certificate_path: None,
            agent_backend: Default::default(),
            agent_identity: None,
            route: cshell_domain::SshRoute::Direct,
        };
        let view = DesktopProfileView {
            catalog: Some(DesktopProfileCatalog {
                revision: 7,
                defaults: Default::default(),
                folders: vec![],
                profiles: vec![],
                ssh_connections: vec![target.clone()],
                local_connections: vec![],
                themes: vec![],
            }),
            host_key_scope: Some((target.profile_id, 7)),
            host_key_preview: Some(HostKeyPreviewData {
                profile_id: target.profile_id.as_uuid().as_bytes().to_vec(),
                host: target.host.clone(),
                port: 22,
                algorithm: "ssh-ed25519".into(),
                fingerprint: "SHA256:test-only-fingerprint".into(),
                public_key_line: "ssh-ed25519 test-only-key".into(),
                token: vec![4; 32],
                expires_unix_seconds: u64::MAX,
            }),
            ..Default::default()
        };
        (HostKeyPrompt::new(target, 7, true), view)
    }

    fn frame(
        context: &egui::Context,
        prompt: &mut HostKeyPrompt,
        events: Vec<egui::Event>,
    ) -> (egui::accesskit::TreeUpdate, Option<Action>) {
        let mut action = None;
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(720.0, 480.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                action = prompt.draw(ui.ctx());
            },
        );
        output.textures_delta.clear();
        (output.platform_output.accesskit_update.unwrap(), action)
    }

    fn click(tree: &egui::accesskit::TreeUpdate, label: &str) -> Vec<egui::Event> {
        let node = &tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label))
            .unwrap()
            .1;
        let b = node.bounds().unwrap();
        let pos = egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32);
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
    fn one_click_confirms_the_displayed_key_and_reconnects_only_after_receipt() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let (mut prompt, mut view) = fixture();
        let (_, action) = frame(&context, &mut prompt, vec![]);
        assert!(matches!(
            action,
            Some(Action::Request(ProfileClientCommand::PreviewHostKey { .. }))
        ));
        prompt.sync(&view);
        frame(&context, &mut prompt, vec![]);
        let (tree, action) = frame(&context, &mut prompt, vec![]);
        assert!(
            action.is_none(),
            "displaying a fingerprint must not trust it"
        );
        assert!(
            !tree
                .nodes
                .iter()
                .any(|(_, node)| node.role() == egui::accesskit::Role::TextInput)
        );
        let events = click(&tree, "确认并连接");
        let (_, action) = frame(&context, &mut prompt, events.clone());
        match action {
            Some(Action::Request(ProfileClientCommand::ConfirmHostKey {
                profile_id,
                expected_revision,
                token,
                fingerprint,
            })) => {
                let preview = view.host_key_preview.as_ref().unwrap();
                assert_eq!(profile_id, prompt.target.profile_id);
                assert_eq!(expected_revision, 7);
                assert_eq!(&*token, &preview.token);
                assert_eq!(fingerprint, preview.fingerprint);
            }
            _ => panic!("one click must submit the exact displayed key"),
        }
        assert!(
            frame(&context, &mut prompt, events).1.is_none(),
            "do not submit twice or reconnect early"
        );
        view.host_key_preview = None;
        view.host_key_confirmed = Some(prompt.target.profile_id);
        prompt.sync(&view);
        assert!(
            matches!(prompt.draw(&context), Some(Action::Connect(id)) if id == prompt.target.profile_id)
        );
    }

    #[test]
    fn cancel_and_stale_preview_never_trust_or_connect() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let (mut prompt, view) = fixture();
        assert_eq!(prompt.cancelled_connection_profile(),Some(prompt.target.profile_id));
        let preview=HostKeyPrompt::new(prompt.target.clone(),7,false);assert_eq!(preview.cancelled_connection_profile(),None);
        frame(&context, &mut prompt, vec![]);
        prompt.sync(&view);
        frame(&context, &mut prompt, vec![]);
        let (tree, _) = frame(&context, &mut prompt, vec![]);
        assert!(matches!(
            frame(&context, &mut prompt, click(&tree, "取消")).1,
            Some(Action::Dismiss)
        ));
        for expired in [false, true] {
            let (mut prompt, mut view) = fixture();
            frame(&context, &mut prompt, vec![]);
            if expired {
                view.host_key_preview.as_mut().unwrap().expires_unix_seconds = 0;
            } else {
                view.catalog.as_mut().unwrap().revision += 1;
            }
            prompt.sync(&view);
            let (_, action) = frame(&context, &mut prompt, vec![]);
            assert!(action.is_none());
            assert!(matches!(prompt.stage, Stage::Failed(_)));
        }
    }

    #[test]
    fn existing_entry_is_informational_and_never_imports_or_connects() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let (mut prompt, mut view) = fixture();
        prompt.reconnect = false;
        prompt.preview_started();
        view.host_key_preview = None;
        view.host_key_existing_entry = true;
        prompt.sync(&view);
        assert!(matches!(prompt.stage, Stage::ExistingEntry));
        frame(&context, &mut prompt, vec![]);
        let (tree, action) = frame(&context, &mut prompt, vec![]);
        assert!(action.is_none());
        assert!(!tree.nodes.iter().any(|(_, node)| node.label().is_some_and(|label| label.contains("确认并"))));
        assert!(matches!(frame(&context, &mut prompt, click(&tree, "关闭")).1, Some(Action::Dismiss)));
        for error in ["HostKeyChanged", "HostKeyRevoked", "scan failed"] {
            let (mut prompt, mut view) = fixture();
            prompt.preview_started();
            view.error = Some(error.into());
            prompt.sync(&view);
            assert!(matches!(prompt.stage, Stage::Failed(_)));
        }
    }
}

pub(crate) enum Action {
    Request(ProfileClientCommand),
    Connect(ProfileId),
    Dismiss,
}

pub(crate) struct HostKeyPrompt {
    target: SshConnectionRecord,
    revision: u64,
    reconnect: bool,
    stage: Stage,
}

impl HostKeyPrompt {
    pub(crate) fn new(target: SshConnectionRecord, revision: u64, reconnect: bool) -> Self {
        Self {
            target,
            revision,
            reconnect,
            stage: Stage::Start,
        }
    }

    pub(crate) fn fail(&mut self, message: impl Into<String>) {
        self.stage = Stage::Failed(message.into());
    }

    pub(crate) fn preview_started(&mut self) {
        self.stage = Stage::Scanning;
    }

    pub(crate) fn sync(&mut self, view: &DesktopProfileView) {
        if !matches!(
            self.stage,
            Stage::Scanning | Stage::Ready(_) | Stage::Confirming
        ) {
            return;
        }
        if !view.catalog.as_ref().is_some_and(|catalog| {
            catalog.revision == self.revision
                && catalog
                    .ssh_connections
                    .iter()
                    .any(|target| target == &self.target)
        }) {
            self.fail("连接配置已变化，请取消后重新连接。");
            return;
        }
        if view.host_key_pending
            || view.host_key_scope != Some((self.target.profile_id, self.revision))
        {
            return;
        }
        if let Some(error) = &view.error {
            self.fail(error.clone());
        } else if matches!(self.stage, Stage::Scanning) && view.host_key_existing_entry {
            self.stage = Stage::ExistingEntry;
        } else if matches!(self.stage, Stage::Confirming) {
            if view.host_key_confirmed == Some(self.target.profile_id) {
                self.stage = Stage::Complete;
            }
        } else if let Some(preview) = &view.host_key_preview {
            if preview.profile_id == self.target.profile_id.as_uuid().as_bytes()
                && preview.host == self.target.host
                && preview.port == u32::from(self.target.port)
                && !preview.token.is_empty()
                && !preview.fingerprint.is_empty()
            {
                self.stage = Stage::Ready(preview.clone());
            } else {
                self.fail("服务器密钥预览与连接目标不匹配，请重新连接。");
            }
        }
    }

    pub(crate) fn draw(&mut self, context: &egui::Context) -> Option<Action> {
        if matches!(self.stage, Stage::Complete) {
            return Some(if self.reconnect {
                Action::Connect(self.target.profile_id)
            } else {
                Action::Dismiss
            });
        }
        let mut action = None;
        if matches!(self.stage, Stage::Start) {
            self.stage = Stage::Scanning;
            action = Some(Action::Request(ProfileClientCommand::PreviewHostKey {
                profile_id: self.target.profile_id,
                expected_revision: self.revision,
            }));
        }
        if let Stage::Ready(preview) = &self.stage {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(u64::MAX, |duration| duration.as_secs());
            if now >= preview.expires_unix_seconds {
                self.fail("密钥预览已过期，请重试。");
            }
        }
        let response =
            egui::Modal::new(egui::Id::new("cshell-host-key-confirmation")).show(context, |ui| {
                ui.set_width((context.content_rect().width() - 48.0).clamp(260.0, 480.0));
                ui.heading(if matches!(self.stage, Stage::ExistingEntry) {
                    "已有服务器密钥记录"
                } else {
                    "信任此服务器？"
                });
                ui.label(format!("{}:{}", self.target.host, self.target.port));
                match &self.stage {
                    Stage::ExistingEntry => {
                        ui.label("此主机已有密钥记录，无需再次导入。");
                        ui.label("连接时会自动校验服务器密钥。若密钥变更或已撤销，将拒绝连接。");
                        if ui.button("关闭").clicked() {
                            action = Some(Action::Dismiss);
                        }
                    }
                    Stage::Ready(preview) => {
                        ui.label(format!("密钥算法：{}", preview.algorithm));
                        ui.label("SHA256 指纹");
                        ui.add(
                            egui::Label::new(egui::RichText::new(&preview.fingerprint).monospace())
                                .wrap(),
                        );
                        ui.label("首次连接此服务器。确认后保存此密钥，今后连接自动校验。");
                        ui.horizontal(|ui| {
                            if ui.button("取消").clicked() {
                                action = Some(Action::Dismiss);
                            }
                            if ui
                                .button(if self.reconnect {
                                    "确认并连接"
                                } else {
                                    "确认并保存"
                                })
                                .clicked()
                            {
                                action =
                                    Some(Action::Request(ProfileClientCommand::ConfirmHostKey {
                                        profile_id: self.target.profile_id,
                                        expected_revision: self.revision,
                                        token: Zeroizing::new(preview.token.clone()),
                                        fingerprint: preview.fingerprint.clone(),
                                    }));
                            }
                        });
                    }
                    Stage::Failed(error) => {
                        ui.colored_label(cshell_ui::theme::DANGER, error);
                        if ui.button("取消").clicked() {
                            action = Some(Action::Dismiss);
                        }
                    }
                    _ => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(if matches!(self.stage, Stage::Confirming) {
                                "正在保存服务器密钥…"
                            } else {
                                "正在获取服务器密钥…"
                            });
                        });
                        if matches!(self.stage, Stage::Scanning) && ui.button("取消").clicked() {
                            action = Some(Action::Dismiss);
                        }
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_millis(100));
                    }
                }
            });
        if response.should_close() && !matches!(self.stage, Stage::Confirming) {
            action = Some(Action::Dismiss);
        }
        if matches!(
            action,
            Some(Action::Request(ProfileClientCommand::ConfirmHostKey { .. }))
        ) {
            self.stage = Stage::Confirming;
        }
        action
    }
}
