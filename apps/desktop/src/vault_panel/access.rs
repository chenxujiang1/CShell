use super::*;
use cshell_ipc::VaultControlStatus;

impl VaultPanel {
    pub(super) fn request(&self, operation: Op, selected: Option<(ProfileId, u64)>) -> VaultRequest {
        let mut request = VaultRequest::new(operation);
        if let Some((id, revision)) = selected {
            request.profile_id = id.as_uuid().as_bytes().to_vec();
            request.expected_profile_revision = revision;
        }
        request.expected_vault_revision = self.response.as_ref().map_or(0, |r| r.revision);
        request.expected_lock_generation = self.response.as_ref().map_or(0, |r| r.lock_generation);
        request
    }
    fn protection_choice(&mut self, ui: &mut egui::Ui, allow_system: bool) {
        let previous = self.creation_system;
        if allow_system {
            ui.label("保护方式");
            ui.horizontal_wrapped(|ui| {
                ui.radio_value(&mut self.creation_system, false, "自定义主密码");
                ui.radio_value(&mut self.creation_system, true, "系统钥匙串");
            });
        } else {
            self.creation_system = false;
        }
        if previous != self.creation_system {
            self.clear();
        }
        if self.creation_system {
            ui.weak("由当前系统账户的钥匙串保护，无需另设主密码；迁移到其他设备前请导出密钥。");
        } else {
            ui.label("新主密码（8～1024 字节）");
            crate::secret_edit::edit(ui, &mut self.password, "vault-password", 1024);
            ui.label("再次输入新主密码");
            crate::secret_edit::edit(ui, &mut self.confirmation, "vault-confirmation", 1024);
            ui.weak("主密码无法找回，请妥善保存；它与单个私钥的口令相互独立。");
            if !self.confirmation.is_empty() && *self.password != *self.confirmation {
                ui.colored_label(cshell_ui::theme::WARNING, "两次输入的主密码不一致");
            }
        }
    }
    pub(super) fn new_password_valid(&self) -> bool {
        self.password.len() >= 8 && *self.password == *self.confirmation
    }
    pub(super) fn draw_access(
        &mut self,
        ui: &mut egui::Ui,
        selected: Option<(ProfileId, u64)>,
    ) -> Option<VaultRequest> {
        let Some(response) = self.response.clone() else {
            ui.label("正在获取凭据库状态…");
            return ui
                .button("刷新状态")
                .clicked()
                .then(|| self.request(Op::Status, selected));
        };
        let mut outgoing = None;
        ui.horizontal_wrapped(|ui| {
            ui.strong(if !response.exists {
                "尚未创建凭据库"
            } else if response.unlocked {
                "凭据库已解锁"
            } else if response.status == VaultControlStatus::Ok as i32 {
                "凭据库已锁定"
            } else {
                "凭据库暂不可用"
            });
            if response.exists {
                ui.label(if response.system {
                    "保护方式：系统钥匙串"
                } else {
                    "保护方式：主密码"
                });
            }
            if ui.button("刷新状态").clicked() {
                outgoing = Some(self.request(Op::Status, selected));
            }
            if response.unlocked && ui.button("立即锁定").clicked() {
                self.clear();
                outgoing = Some(self.request(Op::Lock, selected));
            }
        });
        if !response.storage_path.is_empty() {
            ui.collapsing("存储位置", |ui| {
                ui.label(&response.storage_path);
            });
        }
        if !response.unlocked && !response.key_notice.is_empty() {
            ui.weak(&response.key_notice);
        }
        if self.busy {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("正在处理，请稍候…");
            });
            return outgoing;
        }
        if !response.exists {
            self.protection_choice(ui, true);
            let valid = self.creation_system || self.new_password_valid();
            let label = if self.creation_system {
                "创建系统钥匙串保护的凭据库"
            } else {
                "创建并解锁凭据库"
            };
            if ui.add_enabled(valid, egui::Button::new(label)).clicked() {
                let mut request = self.request(
                    if self.creation_system {
                        Op::CreateSystem
                    } else {
                        Op::Create
                    },
                    selected,
                );
                if !self.creation_system {
                    request.secret = std::mem::take(&mut *self.password).into_bytes();
                }
                self.clear();
                outgoing = Some(request);
            }
            return outgoing;
        }
        if response.unlocked {
            ui.weak("本次运行期间保持解锁，锁屏、切换应用及闲置不会自动锁定。可手动锁定，退出程序后解锁状态会清除。");
            if self.page == 1 && !response.system {
                ui.collapsing("更换主密码", |ui| {
                    self.creation_system = false;
                    self.protection_choice(ui, false);
                    if ui
                        .add_enabled(self.new_password_valid(), egui::Button::new("更换主密码"))
                        .clicked()
                    {
                        let mut request = self.request(Op::ChangePassword, selected);
                        request.secret = std::mem::take(&mut *self.password).into_bytes();
                        self.clear();
                        outgoing = Some(request);
                    }
                });
            }
            return outgoing;
        }
        if !self.rebuilding {
            if response.system {
                ui.weak("使用当前系统账户解锁，无需输入主密码。");
                if ui.button("通过系统钥匙串解锁").clicked() {
                    outgoing = Some(self.request(Op::UnlockSystem, selected));
                }
            } else {
                ui.label("主密码");
                crate::secret_edit::edit(ui, &mut self.password, "vault-password", 1024);
                if ui
                    .add_enabled(!self.password.is_empty(), egui::Button::new("解锁"))
                    .clicked()
                {
                    let mut request = self.request(Op::Unlock, selected);
                    request.secret = std::mem::take(&mut *self.password).into_bytes();
                    self.clear();
                    outgoing = Some(request);
                }
            }
            ui.horizontal_wrapped(|ui| {
                if ui.button("重新加载（保持锁定）").clicked() {
                    self.clear();
                    outgoing = Some(self.request(Op::Reload, selected));
                }
                if ui.button("无法解锁？重建凭据库…").clicked() {
                    self.clear();
                    self.rebuilding = true;
                }
            });
        } else {
            ui.separator();
            ui.strong("保留旧库并重建");
            ui.colored_label(
                cshell_ui::theme::WARNING,
                "这会创建空的新凭据库，不能恢复旧库中无法解密的密码和私钥。",
            );
            ui.label("旧库的加密文件与密钥标识完整保留。SSH 连接配置、导出的密钥和服务器公钥保持不变；新库中的凭据需重新填写或导入。");
            self.protection_choice(ui, true);
            ui.checkbox(
                &mut self.rebuild_confirmed,
                "我理解新库为空，需要重新添加凭据，旧库不会自动恢复",
            );
            let valid = self.rebuild_confirmed
                && (self.creation_system || self.new_password_valid())
                && response.status != VaultControlStatus::Busy as i32;
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(valid, egui::Button::new("保留旧库并重建"))
                    .clicked()
                {
                    let mut request = self.request(
                        if self.creation_system {
                            Op::RebuildSystem
                        } else {
                            Op::RebuildPassword
                        },
                        None,
                    );
                    request.confirm_rebuild = true;
                    if !self.creation_system {
                        request.secret = std::mem::take(&mut *self.password).into_bytes();
                    }
                    self.clear();
                    outgoing = Some(request);
                }
                if ui.button("取消重建").clicked() {
                    self.clear();
                    self.rebuilding = false;
                }
            });
        }
        outgoing
    }
}
