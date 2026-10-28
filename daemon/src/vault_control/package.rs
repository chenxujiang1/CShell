use super::*;
use cshell_ipc::{ProfileImportAction as Action, ProfileImportPolicy as Policy};
use cshell_vault::{CredentialPackageRecord, KeychainError};

impl VaultController {
    pub(crate) fn export_records(
        &self,
        targets: &[SshConnectionRecord],
        include_keys: bool,
        revision: u64,
        generation: u64,
    ) -> Result<Vec<CredentialPackageRecord>, VaultError> {
        self.ensure_owned()?;
        let mut state = self.guard();
        if self.epoch.load(Ordering::Acquire) != generation {
            return Err(VaultError::AlreadyExists);
        }
        if let Backend::Portable(vault) = &state.backend {
            check_revision(vault, revision)?;
        }
        let mut records = Vec::new();
        for target in targets {
            let encrypted = mode(&mut state, target.profile_id)?;
            let mut kinds = Vec::new();
            if target.uses_password() { kinds.push(Kind::Password); }
            if target.uses_private_key() {
                kinds.push(Kind::KeyPassphrase);
                if include_keys { kinds.push(Kind::PrivateKey); }
            }
            for kind in kinds {
                let value = if encrypted {
                    portable(&mut state)?
                        .read(reference(target.profile_id, kind), &binding(target, kind)?)
                } else {
                    native_read(target, kind)
                };
                match value {
                    Ok(secret) => records.push(CredentialPackageRecord::new(
                        reference(target.profile_id, kind),
                        binding(target, kind)?,
                        secret,
                    )),
                    Err(VaultError::Missing) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if records.is_empty() {
            return Err(VaultError::Missing);
        }
        if self.epoch.load(Ordering::Acquire) != generation {
            return Err(VaultError::Locked);
        }
        Ok(records)
    }

    pub(crate) fn package_actions(
        &self,
        ids: &[ProfileId],
        policy: Policy,
        revision: u64,
        generation: u64,
    ) -> Result<Vec<Action>, VaultError> {
        self.ensure_owned()?;
        let mut state = self.guard();
        if self.epoch.load(Ordering::Acquire) != generation {
            return Err(VaultError::AlreadyExists);
        }
        let vault = portable(&mut state)?;
        check_revision(vault, revision)?;
        ids.iter()
            .map(|&id| package_action(vault, id, policy))
            .collect()
    }

    /// Final bounded file commit only. Password derivation must finish before acquiring this gate.
    pub(crate) fn restore_package(
        &self,
        records: &[CredentialPackageRecord],
        policy: Policy,
        revision: u64,
        generation: u64,
    ) -> Result<(), VaultError> {
        self.ensure_owned()?;
        // Wait behind any KDF before taking the publication gate, then verify
        // the generation so a manual lock or program exit rejects stale restores.
        let mut state = self.guard();
        let _gate = self
            .commit_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.epoch.load(Ordering::Acquire) != generation {
            return Err(VaultError::AlreadyExists);
        }
        let vault = portable(&mut state)?;
        check_revision(vault, revision)?;
        let ids: std::collections::BTreeSet<_> = records
            .iter()
            .map(|r| ProfileId::from_bytes(r.reference.profile_id))
            .collect();
        let mut selected = std::collections::BTreeSet::new();
        for id in ids {
            match package_action(vault, id, policy)? {
                Action::Conflict => return Err(VaultError::AlreadyExists),
                Action::Skip => {}
                _ => {
                    selected.insert(id);
                }
            }
        }
        if selected.is_empty() {
            return Ok(());
        }
        let modes: Vec<_> = selected
            .iter()
            .map(|&id| (reference(id, Kind::ProfileMode), Secret::new(vec![1])))
            .collect();
        let mut entries: Vec<_> = records
            .iter()
            .filter(|r| selected.contains(&ProfileId::from_bytes(r.reference.profile_id)))
            .map(|r| (r.reference, r.binding(), &r.secret))
            .collect();
        entries.extend(modes.iter().map(|(id, value)| (*id, MODE_BINDING, value)));
        // Replace a whole credential group. Absent fields are removed to avoid mixing secret sets.
        let removals: Vec<_> = selected
            .iter()
            .flat_map(|&id| {
                [Kind::Password, Kind::KeyPassphrase, Kind::PrivateKey]
                    .into_iter()
                    .map(move |kind| reference(id, kind))
            })
            .filter(|id| !entries.iter().any(|(entry, _, _)| entry == id))
            .collect();
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        self.changes.send_replace(epoch);
        vault.update_batch(&entries, &removals)
    }
}
fn package_action(
    vault: &PortableVault,
    id: ProfileId,
    policy: Policy,
) -> Result<Action, VaultError> {
    let mut exists = mode_value(vault, id)?;
    for kind in [Kind::Password, Kind::KeyPassphrase, Kind::PrivateKey] {
        exists |= vault.contains(reference(id, kind))?;
    }
    Ok(if !exists {
        Action::Create
    } else {
        match policy {
            Policy::Fail => Action::Conflict,
            Policy::Skip => Action::Skip,
            Policy::Replace => Action::Replace,
        }
    })
}
fn native_read(target: &SshConnectionRecord, kind: Kind) -> Result<Secret, VaultError> {
    let id = *target.profile_id.as_uuid().as_bytes();
    let result = match kind {
        Kind::Password => SystemProfilePasswordVault::new().read(
            &ProfilePasswordRef::from_profile_bytes(id),
            &ProfilePasswordBinding {
                host: target.host.clone(),
                port: target.port,
                username: target.username.clone(),
            },
        ),
        Kind::KeyPassphrase => SystemProfileKeyPassphraseVault::new().read(
            &ProfileKeyPassphraseRef::from_profile_bytes(id),
            target
                .private_key_path
                .as_deref()
                .ok_or(VaultError::InvalidInput)?,
        ),
        _ => return Err(VaultError::Missing),
    };
    result.map_err(|error| match error {
        KeychainError::Missing => VaultError::Missing,
        KeychainError::BindingMismatch | KeychainError::InvalidCredential => {
            VaultError::InvalidInput
        }
        _ => VaultError::Io(std::io::ErrorKind::Other),
    })
}
