use crate::{MAX_SFTP_INLINE_EDIT_BYTES, SftpVerifiedContent, sftp_digest, sftp_remote_path};
use cshell_domain::{SessionId, SftpQueueItemId};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpEditReviewRequest {
    pub revision: u64,
    pub backup: Option<SftpQueueItemId>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpEditRebase {
    pub remote: SftpVerifiedContent,
    pub permissions: Option<u32>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpEditBackup {
    pub item: SftpQueueItemId,
    pub path: String,
    pub content: SftpVerifiedContent,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpEditReview {
    pub session: SessionId,
    pub id: SessionId,
    pub revision: u64,
    pub base_content: SftpVerifiedContent,
    pub base: Option<String>,
    pub remote_content: SftpVerifiedContent,
    pub remote_permissions: Option<u32>,
    pub remote: String,
    pub backups: Vec<SftpEditBackup>,
    pub selected_backup: Option<SftpQueueItemId>,
    pub backup_text: Option<String>,
}
impl SftpEditReview {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.id.as_uuid().is_nil()
            || self.session.as_uuid().is_nil()
            || self.revision == 0
            || self.backups.len() > 8
            || self.selected_backup.is_some() != self.backup_text.is_some()
            || self
                .backups
                .iter()
                .map(|b| b.item)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.backups.len()
            || self.remote_permissions.is_some_and(|p| p > 0o7777)
            || self.remote.len() > MAX_SFTP_INLINE_EDIT_BYTES
            || sftp_digest(self.remote.as_bytes()) != self.remote_content
            || self.base.as_ref().is_some_and(|s| {
                s.len() > MAX_SFTP_INLINE_EDIT_BYTES
                    || sftp_digest(s.as_bytes()) != self.base_content
            })
            || self
                .backups
                .iter()
                .any(|b| b.item.as_uuid().is_nil() || !sftp_remote_path(&b.path, false))
        {
            return Err("invalid editor comparison scope");
        }
        if let Some(text) = &self.backup_text {
            if text.len() > MAX_SFTP_INLINE_EDIT_BYTES
                || !self.backups.iter().any(|b| {
                    Some(b.item) == self.selected_backup
                        && sftp_digest(text.as_bytes()) == b.content
                })
            {
                return Err("invalid backup comparison");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn comparison_rejects_unbound_or_changed_backup_and_text_never_enters_debug() {
        let content = sftp_digest(b"private text");
        let backup = SftpQueueItemId::new();
        let mut value = SftpEditReview {
            session: SessionId::new(),
            id: SessionId::new(),
            revision: 1,
            base_content: content,
            base: Some("private text".into()),
            remote_content: content,
            remote_permissions: Some(0o644),
            remote: "private text".into(),
            backups: vec![SftpEditBackup {
                item: backup,
                path: "/srv/.backup".into(),
                content,
            }],
            selected_backup: Some(backup),
            backup_text: Some("private text".into()),
        };
        value.validate().unwrap();
        assert!(!format!("{value:?}").contains("private text"));
        value.backup_text = Some("changed".into());
        assert!(value.validate().is_err());
        value.backup_text = None;
        assert!(value.validate().is_err());
        value.selected_backup = None;
        value.validate().unwrap();
        value.remote_permissions = Some(0o100644);
        assert!(value.validate().is_err());
    }
}
impl std::fmt::Debug for SftpEditReview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpEditReview")
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}
