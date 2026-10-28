use super::*;
impl VaultPanel {
    pub(crate) fn draw_protection(&mut self, ui: &mut egui::Ui) -> Option<VaultRequest> {
        ui.heading("凭据库与安全");
        let Some(response) = self.response.clone() else {
            ui.label("正在读取凭据库状态…");
            return ui
                .button("刷新凭据库状态")
                .clicked()
                .then(|| VaultRequest::new(Op::Status));
        };
        ui.label(format!(
            "当前保护方式：{} · {}",
            if !response.exists {
                "系统托管（默认）"
            } else if response.system {
                "系统托管"
            } else {
                "自定义主密码"
            },
            if response.unlocked {
                "已解锁"
            } else if response.exists {
                "已锁定"
            } else {
                "尚未创建凭据库"
            }
        ));
        ui.weak("系统托管：连接时按需自动解锁。主密码：需要时在连接弹框输入，随后自动继续。解锁后本次运行期间持续有效，锁屏、切换应用及闲置均不锁库；仅手动锁定或退出程序时清除解锁状态。");
        if let Some(error) = &self.error {
            ui.colored_label(cshell_ui::theme::WARNING, error);
        }
        if !self.notice.is_empty() {
            ui.label(&self.notice);
        }
        if response.protection_cleanup_pending {
            ui.colored_label(
                cshell_ui::theme::WARNING,
                "旧系统密钥尚未清理，完成清理后才能再次切换保护方式。",
            );
            if !response.unlocked {
                let action = self.draw_access(ui, None);
                if action.is_some() {
                    self.busy = true;
                }
                return action;
            }
            return ui
                .add_enabled(!self.busy, egui::Button::new("完成旧系统密钥清理"))
                .clicked()
                .then(|| self.request(Op::FinishProtectionCleanup, None));
        }
        if let Some(request) = self.draw_access(ui, None) {
            self.busy = true;
            return Some(request);
        }
        if !response.exists || !response.unlocked || self.busy {
            return None;
        }
        let choice = self.protection_target.get_or_insert(response.system);
        let previous = *choice;
        egui::ComboBox::from_id_salt("vault-global-protection")
            .selected_text(if *choice {
                "系统托管（推荐）"
            } else {
                "自定义主密码"
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(choice, true, "系统托管（推荐）");
                ui.selectable_value(choice, false, "自定义主密码");
            });
        let system = *choice;
        if previous != system {
            self.clear();
        }
        if system != response.system {
            if system {
                ui.label("验证当前主密码");
                crate::secret_edit::edit(
                    ui,
                    &mut self.password,
                    "protection-current-password",
                    1024,
                );
                ui.weak("验证通过后，将保留全部凭据并改用当前系统账户保护。");
            } else {
                ui.label("设置新主密码（至少 8 字节）");
                crate::secret_edit::edit(ui, &mut self.password, "protection-new-password", 1024);
                ui.label("再次输入新主密码");
                crate::secret_edit::edit(
                    ui,
                    &mut self.confirmation,
                    "protection-confirm-password",
                    1024,
                );
                ui.weak("主密码丢失无法找回，建议提前导出加密备份。");
            }
            let valid = if system {
                !self.password.is_empty()
            } else {
                self.new_password_valid()
            };
            if ui
                .add_enabled(valid, egui::Button::new("应用保护方式（保留所有凭据）"))
                .clicked()
            {
                let mut request = self.request(
                    if system {
                        Op::ProtectWithSystem
                    } else {
                        Op::ProtectWithPassword
                    },
                    None,
                );
                request.secret = std::mem::take(&mut *self.password).into_bytes();
                self.clear();
                self.busy = true;
                self.notice.clear();
                return Some(request);
            }
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("立即锁定").clicked() {
                self.busy = true;
            }
        });
        if self.busy {
            return Some(self.request(Op::Lock, None));
        }
        ui.weak(
            "保护方式只作用于当前库。会话外部凭据和已有备份不迁移、不删除；已连接会话保持运行。",
        );
        None
    }
}

#[cfg(test)]
mod tests;
