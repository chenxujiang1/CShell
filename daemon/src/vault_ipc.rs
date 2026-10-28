use crate::ProfileIpcService;
use cshell_domain::{ProfileId, SshAuthMethod};
use cshell_ipc::{
    VaultControlStatus as Status, VaultOperation as Op, VaultRequest, VaultResponse, decode_id,
};
use cshell_vault::{Secret, VaultError};
use zeroize::Zeroizing;

impl ProfileIpcService {
    pub(crate) async fn handle_vault(&self, mut request: VaultRequest) -> VaultResponse {
        let operation = match Op::try_from(request.operation) {
            Ok(value) => value,
            Err(_) => return failure(Status::Invalid),
        };
        if matches!(operation, Op::RebuildPassword | Op::RebuildSystem) {
            return self.handle_rebuild(request, operation == Op::RebuildSystem).await;
        }
        if matches!(operation,Op::ProtectWithSystem|Op::ProtectWithPassword|Op::FinishProtectionCleanup) {return self.handle_protection(request,operation).await;}
        if request.confirm_rebuild { return failure(Status::Invalid); }
        if matches!(operation, Op::GenerateManagedKey | Op::ImportManagedKey | Op::ExportManagedKey | Op::DeleteManagedKey) {
            return self.handle_managed_key(request, operation).await;
        }
        if request.managed_key.is_some() { return failure(Status::Invalid); }
        if matches!(operation, Op::KeyboardAuthPoll | Op::KeyboardAuthRespond) {
            return self.vault.keyboard_auth.handle(&self.vault, operation, request);
        }
        if request.auth_answer.is_some() {return failure(Status::Invalid);}
        if matches!(
            operation,
            Op::ExportPackage | Op::PreviewPackage | Op::CommitPackage
        ) {
            return self.handle_package(request).await;
        }
        if !request.package.is_empty()
            || !request.preview_token.is_empty()
            || !request.selected_profiles.is_empty()
            || request.package_policy != 0
            || request.include_private_keys
        {
            return failure(Status::Invalid);
        }
        let selected = if request.profile_id.is_empty() {
            None
        } else {
            match decode_id(&request.profile_id) {
                Ok(id) => Some(ProfileId::from_bytes(id)),
                Err(_) => return failure(Status::Invalid),
            }
        };
        let needs_target = matches!(
            operation,
            Op::SetMode | Op::ImportPrivateKey | Op::DeletePrivateKey
        );
        let target = if needs_target {
            let Some(id) = selected else {
                return failure(Status::Invalid);
            };
            let snapshot = match self.service.load().await {
                Ok(catalog) => catalog.snapshot(),
                Err(_) => return failure(Status::Unavailable),
            };
            if snapshot.revision != request.expected_profile_revision {
                return failure(Status::Conflict);
            }
            match snapshot
                .ssh_connections
                .into_iter()
                .find(|target| target.profile_id == id)
            {
                Some(target) => Some(target),
                None => return failure(Status::Invalid),
            }
        } else {
            None
        };
        let secret_allowed = matches!(
            operation,
            Op::Create | Op::Unlock | Op::ChangePassword | Op::ImportPrivateKey
        );
        if (!secret_allowed && !request.secret.is_empty())
            || request.secret.len()
                > if operation == Op::ImportPrivateKey {
                    4096
                } else {
                    1024
                }
        {
            return failure(Status::Invalid);
        }
        let secret = Secret::new(std::mem::take(&mut request.secret));
        if matches!(operation, Op::Lock | Op::LockTransient | Op::Reload) {
            self.vault.invalidate();
        }
        if operation == Op::ImportPrivateKey {
            let target = match target {
                Some(target)
                    if target.uses_private_key() =>
                {
                    target
                }
                _ => return failure(Status::Invalid),
            };
            let Some(path) = &target.private_key_path else {
                return failure(Status::Invalid);
            };
            let encoded = match crate::session_ipc::read_ssh_auth_file(path, "private key").await {
                Ok(value) if value.len() <= 64 * 1024 => value,
                _ => return failure(Status::Invalid),
            };
            let controller = self.vault.clone();
            let revision = request.expected_vault_revision;
            let result = tokio::task::spawn_blocking(move || {
                let phrase = if secret.expose().is_empty() {
                    None
                } else {
                    Some(Zeroizing::new(
                        std::str::from_utf8(secret.expose())
                            .map_err(|_| VaultError::InvalidInput)?
                            .to_owned(),
                    ))
                };
                cshell_ssh::SshPrivateKey::decode_private_key(
                    &encoded,
                    phrase.as_deref().map(|value| value.as_str()),
                )
                .map_err(|error| match error {
                    cshell_ssh::SshError::Ppk(cshell_ssh::PpkError::Busy)
                    | cshell_ssh::SshError::LegacyPem(cshell_ssh::LegacyPemError::Busy)
                    | cshell_ssh::SshError::RsaKey(cshell_ssh::RsaKeyError::Busy) => {
                        VaultError::Busy
                    }
                    _ => VaultError::InvalidInput,
                })?;
                controller.import_key(
                    &target,
                    revision,
                    &Secret::new(encoded.as_bytes().to_vec()),
                    (!secret.expose().is_empty()).then_some(&secret),
                )
            })
            .await;
            return self.vault_result(selected, result).await;
        }
        let controller = self.vault.clone();
        let vault_revision = request.expected_vault_revision;
        let generation = request.expected_lock_generation;
        let portable = request.portable;
        let result = tokio::task::spawn_blocking(move || match operation {
            Op::Status => Ok(()),
            Op::CreateSystem => controller.create_system(generation),
            Op::UnlockSystem => controller.unlock_system(generation),
            Op::Create => controller.create(&secret, generation),
            Op::Unlock => controller.unlock(&secret, generation),
            Op::Lock => {
                controller.lock_manually();
                Ok(())
            }
            Op::LockTransient => {controller.lock();Ok(())},
            Op::Reload => controller.reload(),
            Op::ChangePassword => controller.rotate(vault_revision, &secret, generation),
            Op::SetMode => controller.set_mode(
                selected.ok_or(VaultError::InvalidInput)?,
                vault_revision,
                portable,
            ),
            Op::DeletePrivateKey => {
                controller.delete_key(selected.ok_or(VaultError::InvalidInput)?, vault_revision)
            }
            Op::ProtectWithSystem | Op::ProtectWithPassword | Op::FinishProtectionCleanup | Op::RebuildPassword | Op::RebuildSystem | Op::GenerateManagedKey | Op::ImportManagedKey | Op::ExportManagedKey | Op::DeleteManagedKey | Op::KeyboardAuthPoll | Op::KeyboardAuthRespond | Op::ImportPrivateKey | Op::ExportPackage | Op::PreviewPackage | Op::CommitPackage => {
                Err(VaultError::InvalidInput)
            }
        })
        .await;
        self.vault_result(selected, result).await
    }
    async fn handle_protection(&self,mut request:VaultRequest,operation:Op)->VaultResponse {
        let mut canonical=VaultRequest::new(operation);canonical.secret=request.secret.clone();canonical.expected_lock_generation=request.expected_lock_generation;canonical.expected_vault_revision=request.expected_vault_revision;
        if request!=canonical || request.secret.len()>1024 || operation==Op::FinishProtectionCleanup && !request.secret.is_empty(){return failure(Status::Invalid);}
        let controller=self.vault.clone();let secret=Secret::new(std::mem::take(&mut request.secret));
        let result=tokio::task::spawn_blocking(move||if operation==Op::FinishProtectionCleanup{controller.finish_protection_cleanup()}else{controller.protect(request.expected_vault_revision,request.expected_lock_generation,&secret,operation==Op::ProtectWithSystem)}).await;
        let mut response=self.vault_result(None,result).await;
        if response.protection_cleanup_pending {response.key_notice="保护方式状态以当前显示为准；旧系统密钥清理未完成，请点击完成清理。".into();}
        else if response.status==Status::Ok as i32 {response.key_notice=if operation==Op::FinishProtectionCleanup {"旧系统解锁密钥已清理，当前凭据库保持可用。"}else{"凭据库保护方式已更新，所有原有凭据及密钥均已保留。"}.into();}
        response
    }
    async fn handle_rebuild(&self, mut request: VaultRequest, system: bool) -> VaultResponse {
        if !request.confirm_rebuild || !request.profile_id.is_empty() || request.expected_profile_revision != 0
            || request.auth_answer.is_some() || request.managed_key.is_some() || !request.package.is_empty()
            || !request.preview_token.is_empty() || !request.selected_profiles.is_empty() || request.package_policy != 0
            || request.include_private_keys || request.portable || request.secret.len() > 1024
            || (system && !request.secret.is_empty()) { return failure(Status::Invalid); }
        let controller = self.vault.clone();
        let secret = Secret::new(std::mem::take(&mut request.secret));
        let result = tokio::task::spawn_blocking(move || controller.rebuild(request.expected_vault_revision,
            request.expected_lock_generation, &secret, system, request.confirm_rebuild)).await;
        let mut response = self.vault_result(None, result).await;
        if response.status == Status::Ok as i32 { response.key_notice = "新凭据库已创建并解锁。旧库文件完整保留，连接配置不变；请重新导入密钥或填写凭据。".into(); }
        response
    }
    async fn handle_managed_key(&self, mut request: VaultRequest, operation: Op) -> VaultResponse {
        if request.auth_answer.is_some() || !request.package.is_empty() || !request.preview_token.is_empty()
            || !request.selected_profiles.is_empty() || request.package_policy != 0 || request.include_private_keys
            || request.portable || request.secret.len() > 1024 { return failure(Status::Invalid); }
        let selected = if request.profile_id.is_empty() { None } else {
            match decode_id(&request.profile_id) { Ok(id) => Some(ProfileId::from_bytes(id)), Err(_) => return failure(Status::Invalid) }
        };
        let Some(key) = request.managed_key.take() else { return failure(Status::Invalid); };
        let controller = self.vault.clone();
        let secret = Secret::new(std::mem::take(&mut request.secret));
        let result = tokio::task::spawn_blocking(move || controller.managed_key_operation(
            operation, &key, &secret, request.expected_vault_revision, request.expected_lock_generation)).await;
        let controller = self.vault.clone();
        let mut response = tokio::task::spawn_blocking(move || controller.response(selected)).await
            .unwrap_or_else(|_| failure(Status::Unavailable));
        match result {
            Ok(Ok(notice)) => response.key_notice = notice.into(),
            Ok(Err(error)) => { response.status = crate::vault_control::status(error.0) as i32; response.key_notice = error.1.into(); }
            Err(_) => { response.status = Status::Unavailable as i32; response.key_notice = "密钥操作失败，请刷新凭据库状态后重试".into(); }
        }
        response
    }
    async fn vault_result(
        &self,
        selected: Option<ProfileId>,
        result: Result<Result<(), VaultError>, tokio::task::JoinError>,
    ) -> VaultResponse {
        let controller = self.vault.clone();
        let mut response = tokio::task::spawn_blocking(move || controller.response(selected))
            .await
            .unwrap_or_else(|_| failure(Status::Unavailable));
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => response.status = crate::vault_control::status(error) as i32,
            Err(_) => response.status = Status::Unavailable as i32,
        }
        response
    }
}
fn failure(status: Status) -> VaultResponse {
    VaultResponse {
        status: status as i32,
        ..Default::default()
    }
}
