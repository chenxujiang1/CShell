use cshell_ipc::{ManagedKeyInfo, ManagedKeyRequest, VaultOperation as Op, VaultRequest};
use std::{path::PathBuf, sync::mpsc};
use zeroize::{Zeroize, Zeroizing};

/// Keep the unlock lease only while our own native picker is in the foreground.
/// Switching from the picker to another application must still lock the Vault.
#[cfg(windows)]
#[allow(unsafe_code)]
pub(crate) fn picker_is_foreground() -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };
    // SAFETY: these read-only Win32 calls accept a window handle and writable PID
    // pointer. A disappearing window simply returns a zero process identifier.
    unsafe {
        let window = GetForegroundWindow();
        let mut process_id = 0;
        GetWindowThreadProcessId(window, &mut process_id);
        process_id == std::process::id()
    }
}
#[cfg(not(windows))]
pub(crate) fn picker_is_foreground() -> bool {
    false
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Form {
    #[default]
    None,
    Generate,
    Import,
    Export,
    Delete,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn new_import_does_not_reuse_a_previous_public_key_or_passphrase() {
        let mut panel = ManagedKeysPanel {
            name: "previous".into(),
            private_path: "previous-key".into(),
            public_path: "unrelated.pub".into(),
            phrase: Zeroizing::new("previous-passphrase".into()),
            ..Default::default()
        };
        panel.begin(Form::Import);
        assert!(panel.name.is_empty());
        assert!(panel.private_path.is_empty() && panel.public_path.is_empty());
        assert!(panel.phrase.is_empty());
    }
    #[test]
    fn key_management_forms_fit_with_real_fonts_in_small_windows() {
        use crate::ui_layout_gate::{check_window, context, frame};
        for size in [egui::vec2(900.0, 600.0), egui::vec2(720.0, 480.0)] {
            for form in [
                Form::None,
                Form::Generate,
                Form::Import,
                Form::Export,
                Form::Delete,
            ] {
                let context = context();
                let mut panel = ManagedKeysPanel {
                    form,
                    protect: true,
                    name: "测试密钥".into(),
                    ..Default::default()
                };
                let keys = vec![ManagedKeyInfo {
                    id: vec![1; 16],
                    name: "Linux 服务器密钥".into(),
                    algorithm: "ssh-ed25519".into(),
                    public_key: format!("ssh-ed25519 {} 测试密钥", "A".repeat(64)),
                    fingerprint: format!("SHA256:{}", "A".repeat(43)),
                    ..Default::default()
                }];
                let mut draw = |c: &egui::Context| {
                    cshell_ui::form::dialog(c, "密钥表单布局")
                        .default_width(800.0)
                        .show(c, |ui| {
                            panel.draw(ui, &keys, true, 1, 2);
                        });
                };
                for _ in 0..2 {
                    frame(&context, size, &mut draw);
                }
                let tree = frame(&context, size, &mut draw);
                check_window(&context, size, "密钥表单布局", &tree);
            }
        }
    }
    fn frame(
        panel: &mut ManagedKeysPanel,
        context: &egui::Context,
        keys: &[ManagedKeyInfo],
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, Option<VaultRequest>) {
        let mut request = None;
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 900.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                request = panel.draw(ui, keys, true, 12, 34);
            },
        );
        output.textures_delta.clear();
        (output, request)
    }
    fn click(output: &egui::FullOutput, label: &str) -> Vec<egui::Event> {
        let bounds = output
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button && node.label() == Some(label)
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
    fn copying_selected_public_key_emits_only_the_authorized_keys_line() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let keys = vec![ManagedKeyInfo {
            id: vec![1; 16],
            name: "test key".into(),
            public_key: "ssh-ed25519 AAAA-test-public-only".into(),
            ..Default::default()
        }];
        let mut panel = ManagedKeysPanel::default();
        frame(&mut panel, &context, &keys, vec![]);
        let (output, _) = frame(&mut panel, &context, &keys, vec![]);
        let (output, request) = frame(&mut panel, &context, &keys, click(&output, "复制公钥"));
        assert!(request.is_none());
        assert!(output.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == &keys[0].public_key)));
    }
    #[test]
    fn generated_key_request_consumes_passphrase_and_fences_revision() {
        let context = egui::Context::default();
        context.enable_accesskit();
        let mut panel = ManagedKeysPanel {
            form: Form::Generate,
            name: "my-key".into(),
            protect: true,
            phrase: Zeroizing::new("test-key-secret".into()),
            confirmation: Zeroizing::new("test-key-secret".into()),
            ..Default::default()
        };
        frame(&mut panel, &context, &[], vec![]);
        let (output, _) = frame(&mut panel, &context, &[], vec![]);
        let (_, request) = frame(&mut panel, &context, &[], click(&output, "生成并保存"));
        let request = request.unwrap();
        assert_eq!(request.operation, Op::GenerateManagedKey as i32);
        assert_eq!(request.expected_vault_revision, 12);
        assert_eq!(request.expected_lock_generation, 34);
        assert_eq!(request.secret, b"test-key-secret");
        assert_eq!(request.managed_key.as_ref().unwrap().algorithm, "ed25519");
        assert!(panel.phrase.is_empty() && panel.confirmation.is_empty());
        assert!(!format!("{request:?}").contains("test-key-secret"));
    }
}
#[derive(Clone, Copy)]
enum Pick {
    Private,
    Public,
    Parent,
}

#[derive(Default)]
pub(crate) struct ManagedKeysPanel {
    selected: Vec<u8>,
    form: Form,
    name: String,
    private_path: String,
    public_path: String,
    export_directory: String,
    protect: bool,
    phrase: Zeroizing<String>,
    confirmation: Zeroizing<String>,
    picker: Option<mpsc::Receiver<(Pick, Option<PathBuf>)>>,
    picker_error: String,
    copied: bool,
}
impl ManagedKeysPanel {
    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
    }
    pub fn clear(&mut self) {
        self.phrase.zeroize();
        self.confirmation.zeroize();
    }
    pub fn finished(&mut self) {
        self.clear();
        self.form = Form::None;
    }
    fn begin(&mut self, form: Form) {
        self.clear();
        if matches!(form, Form::Import | Form::Generate) {
            self.name.clear();
            self.private_path.clear();
            self.public_path.clear();
        }
        self.form = form;
        self.protect = false;
        self.picker_error.clear();
    }
    fn pick(&mut self, kind: Pick) {
        if self.picker.is_some() {
            return;
        }
        let (sender, receiver) = mpsc::channel();
        match std::thread::Builder::new()
            .name("cshell-key-picker".into())
            .spawn(move || {
                let dialog = rfd::FileDialog::new();
                let result = match kind {
                    Pick::Private => dialog.set_title("选择 SSH 私钥文件").pick_file(),
                    Pick::Public => dialog.set_title("选择配对公钥（.pub）").pick_file(),
                    Pick::Parent => dialog
                        .set_title("选择导出父目录，将在其中新建密钥目录")
                        .pick_folder(),
                };
                let _ = sender.send((kind, result));
            }) {
            Ok(_) => self.picker = Some(receiver),
            Err(_) => self.picker_error = "无法打开文件选择器，请直接填写文件路径".into(),
        }
    }
    fn poll_picker(&mut self, context: &egui::Context) {
        let result = self
            .picker
            .as_ref()
            .and_then(|receiver| receiver.try_recv().ok());
        if let Some((kind, path)) = result {
            self.picker = None;
            if let Some(path) = path {
                match kind {
                    Pick::Private => {
                        if self.name.is_empty() {
                            self.name = path
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned();
                        }
                        self.private_path = path.to_string_lossy().into_owned();
                    }
                    Pick::Public => self.public_path = path.to_string_lossy().into_owned(),
                    Pick::Parent => {
                        self.export_directory =
                            path.join("cshell-key").to_string_lossy().into_owned()
                    }
                }
            }
        }
        if self.picker.is_some() {
            context.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
    pub fn draw(
        &mut self,
        ui: &mut egui::Ui,
        keys: &[ManagedKeyInfo],
        ready: bool,
        revision: u64,
        generation: u64,
    ) -> Option<VaultRequest> {
        self.poll_picker(ui.ctx());
        let mut outgoing = None;
        ui.label("SSH 密钥对");
        ui.weak("密钥集中保存在加密凭据库中；公钥可直接复制到服务器的 ~/.ssh/authorized_keys。");
        if !ready {
            self.clear();
            ui.label("请先在“凭据库与连接设置”中创建或解锁凭据库。");
            return None;
        }
        ui.horizontal(|ui| {
            if ui.button("导入密钥对…").clicked() {
                self.begin(Form::Import);
            }
            if ui.button("生成新密钥对…").clicked() {
                self.begin(Form::Generate);
            }
            ui.weak(format!("{} 个密钥", keys.len()));
        });
        ui.separator();
        if !keys.iter().any(|key| key.id == self.selected) {
            self.selected = keys.first().map_or_else(Vec::new, |key| key.id.clone());
        }
        ui.columns(2, |columns| {
            egui::ScrollArea::vertical()
                .id_salt("managed-key-list")
                .max_height(220.0)
                .show(&mut columns[0], |ui| {
                    if keys.is_empty() {
                        ui.weak("尚未添加密钥，可导入现有私钥或生成新密钥。");
                    }
                    for key in keys {
                        if ui
                            .selectable_label(
                                self.selected == key.id,
                                format!(
                                    "{}\n{} · {}",
                                    key.name,
                                    key.algorithm,
                                    if key.encrypted {
                                        "有私钥口令"
                                    } else {
                                        "无私钥口令"
                                    }
                                ),
                            )
                            .clicked()
                        {
                            self.selected = key.id.clone();
                            self.copied = false;
                            self.finished();
                        }
                    }
                });
            if let Some(key) = keys.iter().find(|key| key.id == self.selected) {
                let ui = &mut columns[1];
                ui.strong(&key.name);
                ui.label(&key.fingerprint);
                let mut public = key.public_key.clone();
                ui.add(
                    egui::TextEdit::multiline(&mut public)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(3)
                        .interactive(false),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.button("复制公钥").clicked() {
                        ui.ctx().copy_text(key.public_key.clone());
                        self.copied = true;
                    }
                    if ui.button("导出密钥对…").clicked() {
                        self.begin(Form::Export);
                    }
                    if ui.button("移除…").clicked() {
                        self.begin(Form::Delete);
                    }
                });
                if self.copied {
                    ui.weak("公钥已复制");
                }
                ui.weak("导出后可在 SSH 配置中选择 id_key 作为私钥文件。");
            }
        });
        if self.form == Form::None {
            return None;
        }
        ui.separator();
        let (operation, label) = match self.form {
            Form::Generate => (Op::GenerateManagedKey, "生成并保存"),
            Form::Import => (Op::ImportManagedKey, "导入并保存"),
            Form::Export => (Op::ExportManagedKey, "导出密钥对"),
            Form::Delete => (Op::DeleteManagedKey, "确认移除密钥"),
            Form::None => return None,
        };
        let mut request = ManagedKeyRequest::default();
        let valid = match self.form {
            Form::Generate | Form::Import => {
                cshell_ui::form::text(ui, "密钥名称", &mut self.name);
                self.name.truncate(self.name.floor_char_boundary(128));
                request.name = self.name.trim().to_owned();
                if self.form == Form::Generate {
                    ui.label("算法：Ed25519（OpenSSH）");
                    request.algorithm = "ed25519".into();
                    if ui.checkbox(&mut self.protect, "设置私钥口令").changed() && !self.protect
                    {
                        self.clear();
                    }
                    if self.protect {
                        ui.label("私钥口令");
                        crate::secret_edit::edit(ui, &mut self.phrase, "managed-key-phrase", 1024);
                        ui.label("再次输入私钥口令");
                        crate::secret_edit::edit(
                            ui,
                            &mut self.confirmation,
                            "managed-key-confirm",
                            1024,
                        );
                        if !self.confirmation.is_empty() && *self.phrase != *self.confirmation {
                            ui.colored_label(cshell_ui::theme::WARNING, "两次输入的私钥口令不一致");
                        }
                    } else {
                        ui.weak("私钥不另设口令，仍由加密凭据库保护。导出时为无口令私钥。");
                    }
                } else {
                    ui.label("私钥文件（支持 OpenSSH / PEM / PPK）");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.private_path)
                                .desired_width((ui.available_width() - 115.0).max(60.0)),
                        );
                        if ui.button("选择私钥…").clicked() {
                            self.pick(Pick::Private);
                        }
                    });
                    ui.label("公钥文件（可选，留空时自动从私钥提取）");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.public_path)
                                .desired_width((ui.available_width() - 115.0).max(60.0)),
                        );
                        if ui.button("选择公钥…").clicked() {
                            self.pick(Pick::Public);
                        }
                    });
                    ui.label("现有私钥口令（无口令则留空）");
                    crate::secret_edit::edit(
                        ui,
                        &mut self.phrase,
                        "managed-key-import-phrase",
                        1024,
                    );
                    request.private_path = self.private_path.trim().into();
                    request.public_path = self.public_path.trim().into();
                    ui.weak("导入会校验密钥对，保留输入的口令；原文件保持不变。");
                }
                !request.name.is_empty()
                    && if self.form == Form::Generate {
                        !self.protect
                            || (!self.phrase.is_empty() && *self.phrase == *self.confirmation)
                    } else {
                        !request.private_path.is_empty()
                    }
            }
            Form::Export => {
                request.id = self.selected.clone();
                ui.label("导出到新目录");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.export_directory)
                            .desired_width((ui.available_width() - 115.0).max(60.0)),
                    );
                    if ui.button("选择父目录…").clicked() {
                        self.pick(Pick::Parent);
                    }
                });
                ui.weak("在所选父目录下新建 cshell-key，可修改目录名。导出 id_key（私钥）和 id_key.pub（公钥），保留私钥口令，不覆盖已有目录。");
                request.export_directory = self.export_directory.trim().into();
                !request.id.is_empty() && !request.export_directory.is_empty()
            }
            Form::Delete => {
                request.id = self.selected.clone();
                ui.label("从凭据库移除此密钥对？已导出的文件及服务器上的公钥保持不变。");
                !request.id.is_empty()
            }
            Form::None => false,
        };
        if !self.picker_error.is_empty() {
            ui.colored_label(cshell_ui::theme::DANGER, &self.picker_error);
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(valid && self.picker.is_none(), egui::Button::new(label))
                .clicked()
            {
                let mut message = VaultRequest::new(operation);
                message.expected_vault_revision = revision;
                message.expected_lock_generation = generation;
                message.managed_key = Some(request);
                message.secret = std::mem::take(&mut *self.phrase).into_bytes();
                self.clear();
                outgoing = Some(message);
            }
            if ui.button("取消").clicked() {
                self.finished();
            }
        });
        outgoing
    }
}
