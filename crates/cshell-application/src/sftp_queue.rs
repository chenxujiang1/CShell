//! Session-bound queue metadata. No resume/retry is implied by a saved entry.
use crate::{FilePublicationPolicy, FilePublicationReceipt, UploadContent};
use cshell_domain::{SessionId, SftpQueueItemId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path};
pub const MAX_SFTP_QUEUE_ITEMS: usize = 128;
pub const MAX_SFTP_QUEUE_DOCUMENT: usize = 2 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpQueueDirection {
    Upload,
    Download,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpQueueState {
    Pending,
    Running,
    Succeeded,
    Unknown,
    CleanupRequired,
    Cancelled,
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpQueueDraft {
    #[serde(default,skip_serializing_if="Option::is_none")]
    pub modified_unix_seconds:Option<u32>,
    /// Missing means the original strict algorithm, including for saved queues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<cshell_domain::SftpVerification>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_permissions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_permissions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication_policy: Option<FilePublicationPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_source: Option<SftpVerifiedContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_destination: Option<SftpVerifiedContent>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub follow_links: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "is_false")]
    /// Use the durable staged engine (independent of the chosen checksum policy).
    pub verified: bool,
    pub direction: SftpQueueDirection,
    pub remote_path: String,
    pub local_path: String,
}
impl std::fmt::Debug for SftpQueueDraft {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpQueueDraft")
            .field("direction", &self.direction)
            .field("paths", &"[REDACTED]")
            .finish()
    }
}
impl SftpQueueDraft {
    pub fn managed(&self) -> bool {
        self.modified_unix_seconds.is_some() || self.destination_permissions.is_some()
            || self.expected_permissions.is_some()
            || self.publication_policy.is_some()
            || self.expected_source.is_some()
            || self.expected_destination.is_some()
            || self.follow_links
            || self.rate_limit.is_some()
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.remote_path.len() > 4096
            || !self.remote_path.starts_with('/')
            || self.remote_path.ends_with('/')
            || self.remote_path.contains('\\')
            || self.remote_path.chars().any(char::is_control)
            || self
                .remote_path
                .split('/')
                .skip(1)
                .any(|part| part.is_empty() || part == "." || part == "..")
            || self.local_path.len() > 4096
            || !Path::new(&self.local_path).is_absolute()
            || self.local_path.chars().any(char::is_control)
            || Path::new(&self.local_path).file_name().is_none()
            || self.verified
                && (Path::new(&self.local_path)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                    || self.direction == SftpQueueDirection::Download
                        && self.remote_path.len() > 4000)
            || self.managed() && !self.verified
            || self.verification.is_some() && !self.verified
            || self.expected_destination.is_some() && self.publication_policy.is_none()
            || self
                .expected_source
                .into_iter()
                .chain(self.expected_destination)
                .any(|c| c.size > MAX_SFTP_VERIFIED_BYTES)
            || self
                .rate_limit
                .is_some_and(|r| r == 0 || r > 1024 * 1024 * 1024)
        {
            return Err("invalid queue file paths");
        }
        if self.destination_permissions.is_some_and(|p| p > 0o777)
            || self.expected_permissions.is_some_and(|p| p > 0o7777)
            || (self.destination_permissions.is_some() || self.expected_permissions.is_some())
                && self.direction != SftpQueueDirection::Upload
        {
            return Err("invalid destination permissions");
        }
        Ok(())
    }
}
impl Default for SftpQueueDraft {
    fn default() -> Self {
        Self {
            modified_unix_seconds:None,
            verification: None,
            publication_policy: None,
            destination_permissions: None,
            expected_permissions: None,
            expected_source: None,
            expected_destination: None,
            follow_links: false,
            rate_limit: None,
            verified: false,
            direction: SftpQueueDirection::Upload,
            remote_path: String::new(),
            local_path: String::new(),
        }
    }
}
fn is_false(value: &bool) -> bool {
    !*value
}

pub const MAX_SFTP_VERIFIED_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// Maximum recoverable write intent; old 32 KiB checkpoints remain valid.
pub const MAX_SFTP_WRITE_BATCH_BYTES: u64 = 1024 * 1024;
/// Reserve a bounded recovery range, but keep wire batches at 1 MiB.
pub const MAX_SFTP_CHECKPOINT_BYTES: u64 = 16 * 1024 * 1024;
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpVerifiedContent {
    pub size: u64,
    pub sha256: [u8; 32],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SftpVerifiedIntent {
    Permissions,
    None,
    Create,
    Write,
    Publish,
    Cleanup,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpVerifiedTransfer {
    /// During streaming, content.size is known but its digest has not been computed yet.
    #[serde(default, skip_serializing_if = "is_false")]
    pub digest_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publication: Option<FilePublicationReceipt>,
    pub version: u32,
    pub content: Option<SftpVerifiedContent>,
    pub intent: SftpVerifiedIntent,
    pub bytes: u64,
    pub write_end: Option<u64>,
    pub prefix_sha256: Option<[u8; 32]>,
    pub temporary_owned: bool,
    /// Publication checks passed. Full content verification is selected by draft.verification.
    pub verified: bool,
    pub committed: bool,
    pub cleaned: bool,
}
impl Default for SftpVerifiedTransfer {
    fn default() -> Self {
        Self {
            digest_pending: false,
            publication: None,
            version: 1,
            content: None,
            intent: SftpVerifiedIntent::None,
            bytes: 0,
            write_end: None,
            prefix_sha256: None,
            temporary_owned: false,
            verified: false,
            committed: false,
            cleaned: false,
        }
    }
}
impl SftpQueueItem {
    pub fn backup_path(&self) -> Result<String, &'static str> {
        Ok(self
            .temporary_path()?
            .strip_suffix(".part")
            .ok_or("invalid temporary name")?
            .to_owned()
            + ".bak")
    }
    pub fn temporary_path(&self) -> Result<String, &'static str> {
        let name = format!(".cshell-session-{}.part", self.id);
        let path = match self.draft.direction {
            SftpQueueDirection::Upload => format!(
                "{}/{name}",
                self.draft
                    .remote_path
                    .rsplit_once('/')
                    .ok_or("invalid remote parent")?
                    .0
            ),
            SftpQueueDirection::Download => Path::new(&self.draft.local_path)
                .parent()
                .ok_or("invalid local parent")?
                .join(name)
                .to_str()
                .ok_or("non UTF-8 temporary path")?
                .to_owned(),
        };
        if path.len() > 4096 || path == self.draft.local_path || path == self.draft.remote_path {
            return Err("invalid temporary path");
        }
        Ok(path)
    }
    pub fn can_resume_verified(&self) -> bool {
        self.state == SftpQueueState::Unknown
            && self.verified.as_ref().is_some_and(|v| {
                v.temporary_owned
                    && !v.cleaned
                    && !v.committed
                    && v.content.is_some()
                    && !matches!(
                        v.intent,
                        SftpVerifiedIntent::Publish | SftpVerifiedIntent::Cleanup
                    )
            })
    }
    /// A retry may restart only when no unacknowledged file creation/publication exists.
    pub fn can_restart_verified(&self) -> bool {
        self.state == SftpQueueState::Unknown
            && self.draft.verified
            && self.verified.as_ref().is_some_and(|v| {
                !v.committed
                    && v.intent == SftpVerifiedIntent::None
                    && (v.cleaned || !v.temporary_owned && v.bytes == 0 && !v.verified)
            })
    }
    pub fn can_retry_verified(&self) -> bool {
        self.can_resume_verified() || self.can_restart_verified()
    }
    pub fn can_adopt_verified(&self) -> bool {
        self.state == SftpQueueState::Unknown
            && self.verified.as_ref().is_some_and(|v| {
                v.verified
                    && v.content.is_some()
                    && (v.intent == SftpVerifiedIntent::Publish || v.committed)
            })
    }
    pub fn can_cleanup_verified(&self) -> bool {
        matches!(
            self.state,
            SftpQueueState::Unknown | SftpQueueState::CleanupRequired
        ) && self.verified.as_ref().is_some_and(|v| {
            v.temporary_owned
                && !v.cleaned
                && v.content.is_some()
                && !(v.intent == SftpVerifiedIntent::Publish && !v.committed)
        })
    }
}
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpQueueReceipt {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub bytes: u64,
    pub total: Option<u64>,
    pub destination_committed: bool,
    pub cleanup_path: Option<String>,
    pub cleanup_remote: bool,
}
impl std::fmt::Debug for SftpQueueReceipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SftpQueueReceipt")
            .field("bytes", &self.bytes)
            .field("destination_committed", &self.destination_committed)
            .field("cleanup", &self.cleanup_path.is_some())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpQueueItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified: Option<SftpVerifiedTransfer>,
    pub id: SftpQueueItemId,
    pub draft: SftpQueueDraft,
    pub state: SftpQueueState,
    pub transfer_id: Option<SessionId>,
    pub receipt: Option<SftpQueueReceipt>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpQueueSnapshot {
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub retry_items: BTreeSet<SftpQueueItemId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<cshell_domain::ClipboardTrust>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migrated_to: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_origin: Option<SftpMigrationOrigin>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub migration_digest: Option<[u8; 32]>,
    pub format: String,
    pub version: u32,
    pub session_id: SessionId,
    pub revision: u64,
    pub paused: bool,
    pub items: Vec<SftpQueueItem>,
}
impl SftpQueueSnapshot {
    pub fn initial(session_id: SessionId) -> Self {
        Self {
            retry_items: BTreeSet::new(),
            peer: None,
            concurrency: None,
            migrated_to: None,
            migration_origin: None,
            migration_digest: None,
            format: "cshell.sftp-queue".into(),
            version: 1,
            session_id,
            revision: 1,
            paused: true,
            items: Vec::new(),
        }
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.concurrency.is_some_and(|n| !(1..=4).contains(&n))
            || self
                .migrated_to
                .is_some_and(|s| s == self.session_id || s.as_uuid().is_nil())
            || self
                .migration_origin
                .as_ref()
                .is_some_and(|o| o.session == self.session_id || o.session.as_uuid().is_nil())
            || self.migrated_to.is_some() != self.migration_digest.is_some()
            || self.migrated_to.is_some()
                && (!self.paused
                    || self
                        .items
                        .iter()
                        .any(|i| i.state == SftpQueueState::Running))
            || self.peer.as_ref().is_some_and(|p| {
                p.username.is_empty()
                    || p.username.len() > 1024
                    || p.host_key_sha256.is_empty()
                    || p.host_key_sha256.len() > 256
                    || p.route.len() > 8192
            })
        {
            return Err("invalid queue settings or migration");
        }
        if self.format != "cshell.sftp-queue"
            || self.version != 1
            || self.session_id.as_uuid().is_nil()
            || self.revision == 0
            || self.revision > i64::MAX as u64
            || self.items.len() > MAX_SFTP_QUEUE_ITEMS
            || self
                .items
                .iter()
                .filter(|item| item.state == SftpQueueState::Running)
                .count()
                > self.concurrency.unwrap_or(1) as usize
        {
            return Err("invalid queue snapshot");
        }
        if self.retry_items.iter().any(|id| {
            !self
                .items
                .iter()
                .any(|item| item.id == *id && item.can_retry_verified())
        }) {
            return Err("invalid queued retry");
        }
        let mut ids = BTreeSet::new();
        let mut transfers = BTreeSet::new();
        let mut destinations = BTreeSet::new();
        for item in &self.items {
            item.draft.validate()?;
            if self.concurrency.unwrap_or(1) > 1
                && (matches!(
                    item.state,
                    SftpQueueState::Pending | SftpQueueState::Running
                ) || self.retry_items.contains(&item.id))
            {
                if !item.draft.verified
                    || !destinations.insert((
                        item.draft.direction == SftpQueueDirection::Upload,
                        if item.draft.direction == SftpQueueDirection::Upload {
                            &item.draft.remote_path
                        } else {
                            &item.draft.local_path
                        },
                    ))
                {
                    return Err(
                        "parallel queue requires verified entries and distinct destinations",
                    );
                }
            }
            if item.draft.verified {
                item.temporary_path()?;
            }
            if let Some(v) = &item.verified {
                if item.draft.publication_policy.is_some()
                    && v.content.is_some()
                    && v.publication.is_none()
                {
                    return Err("missing publication plan");
                }
                if let Some(p) = v.publication {
                    p.validate().map_err(|_| "invalid publication receipt")?;
                    if item.draft.publication_policy.is_none() {
                        return Err("unexpected publication receipt");
                    }
                    if v.committed {
                        p.validate_commit(
                            item.draft
                                .publication_policy
                                .ok_or("missing publication policy")?,
                            UploadContent {
                                size: v.content.ok_or("missing publication content")?.size,
                                sha256: v.content.ok_or("missing publication content")?.sha256,
                            },
                        )
                        .map_err(|_| "invalid publication commit")?;
                    }
                }
                if !item.draft.verified
                    || item.transfer_id.is_none()
                    || v.version != 1
                    || matches!(
                        item.state,
                        SftpQueueState::Pending | SftpQueueState::Cancelled
                    )
                    || v.content
                        .is_some_and(|c| c.size > MAX_SFTP_VERIFIED_BYTES || v.bytes > c.size)
                    || v.content.is_none()
                        && (v.bytes != 0
                            || v.temporary_owned
                            || v.verified
                            || v.committed
                            || v.cleaned
                            || v.intent != SftpVerifiedIntent::None
                            || v.prefix_sha256.is_some())
                    || (v.intent == SftpVerifiedIntent::Write) != v.write_end.is_some()
                    || v.write_end.is_some_and(|end| {
                        end <= v.bytes
                            || end - v.bytes > if item.draft.verification.is_some() { MAX_SFTP_CHECKPOINT_BYTES } else { MAX_SFTP_WRITE_BATCH_BYTES }
                            || v.content.is_none_or(|c| end > c.size)
                    })
                    || v.temporary_owned && v.prefix_sha256.is_none()
                    || v.digest_pending && (item.draft.verification.is_none() || v.verified || v.committed || v.content.is_none_or(|c| c.sha256 != [0; 32]))
                    || v.bytes > 0 && !v.temporary_owned
                    || v.intent == SftpVerifiedIntent::Create && v.temporary_owned
                    || v.intent == SftpVerifiedIntent::Write
                        && (!v.temporary_owned || v.verified || v.committed || v.cleaned)
                    || v.verified
                        && (!v.temporary_owned
                            || v.content.is_none_or(|c| {
                                c.size != v.bytes || Some(c.sha256) != v.prefix_sha256
                            }))
                    || v.committed && !v.verified
                    || v.cleaned && v.intent != SftpVerifiedIntent::None
                    || v.cleaned && !v.temporary_owned
                    || v.intent == SftpVerifiedIntent::Publish && !v.verified
                    || v.intent == SftpVerifiedIntent::Cleanup && !v.temporary_owned
                    || matches!(
                        item.state,
                        SftpQueueState::Succeeded | SftpQueueState::CleanupRequired
                    ) && !v.committed
                    || item.state == SftpQueueState::Succeeded && !v.cleaned
                    || item.receipt.as_ref().is_some_and(|r| {
                        r.bytes != v.bytes
                            || r.total != v.content.map(|c| c.size)
                            || r.destination_committed != v.committed
                    })
                {
                    return Err("invalid verified transfer journal");
                }
            } else if item.draft.verified
                && matches!(
                    item.state,
                    SftpQueueState::Running
                        | SftpQueueState::Succeeded
                        | SftpQueueState::CleanupRequired
                )
            {
                return Err("missing verified transfer journal");
            }
            if item.id.as_uuid().is_nil()
                || item.state == SftpQueueState::Running && item.transfer_id.is_none()
                || !ids.insert(item.id)
                || item
                    .transfer_id
                    .is_some_and(|id| id.as_uuid().is_nil() || !transfers.insert(id))
                || matches!(
                    item.state,
                    SftpQueueState::Pending | SftpQueueState::Cancelled
                ) && (item.transfer_id.is_some() || item.receipt.is_some())
                || matches!(
                    item.state,
                    SftpQueueState::Succeeded | SftpQueueState::CleanupRequired
                ) && item
                    .receipt
                    .as_ref()
                    .is_none_or(|r| !r.destination_committed)
                || item.receipt.as_ref().is_some_and(|r| {
                    r.detail
                        .as_ref()
                        .is_some_and(|d| d.len() > 1024 || d.chars().any(char::is_control))
                        || item.state == SftpQueueState::Succeeded
                            && r.total.is_some_and(|t| r.bytes > t)
                        || r.cleanup_path
                            .as_ref()
                            .is_some_and(|p| p.is_empty() || p.len() > 4096 || p.contains('\0'))
                })
            {
                return Err("invalid queue entry");
            }
        }
        if serde_json::to_vec(self)
            .map_err(|_| "invalid queue encoding")?
            .len()
            > MAX_SFTP_QUEUE_DOCUMENT
        {
            return Err("queue document limit");
        }
        Ok(())
    }
    pub fn changed(&mut self) -> Result<(), &'static str> {
        self.revision = self.revision.checked_add(1).ok_or("queue revision limit")?;
        self.validate()
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpMigrationOrigin {
    pub session: SessionId,
    pub digest: [u8; 32],
}
pub fn decode_sftp_queue(bytes: &[u8]) -> Result<SftpQueueSnapshot, &'static str> {
    if bytes.len() > MAX_SFTP_QUEUE_DOCUMENT {
        return Err("queue document limit");
    }
    let value: SftpQueueSnapshot =
        serde_json::from_slice(bytes).map_err(|_| "invalid queue encoding")?;
    value.validate()?;
    Ok(value)
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    #[test]
    fn old_documents_keep_their_encoding_and_strict_policy() {
        let local=if cfg!(windows){"C:/a"}else{"/a"};
        let old=format!(r#"{{"verified":true,"direction":"upload","remote_path":"/a","local_path":"{local}"}}"#);
        let draft:SftpQueueDraft=serde_json::from_str(&old).unwrap();draft.validate().unwrap();
        assert_eq!(draft.verification,None);assert_eq!(serde_json::to_string(&draft).unwrap(),old);
        let old=r#"{"version":1,"content":null,"intent":"none","bytes":0,"write_end":null,"prefix_sha256":null,"temporary_owned":false,"verified":false,"committed":false,"cleaned":false}"#;
        let journal:SftpVerifiedTransfer=serde_json::from_str(old).unwrap();
        assert!(!journal.digest_pending);assert_eq!(serde_json::to_string(&journal).unwrap(),old);
    }
    #[test]
    fn streaming_write_reservations_are_bounded_and_pending_digests_cannot_publish() {
        let mut queue=SftpQueueSnapshot::initial(SessionId::new());
        queue.items.push(SftpQueueItem{
            id:SftpQueueItemId::new(),state:SftpQueueState::Running,transfer_id:Some(SessionId::new()),receipt:None,
            draft:SftpQueueDraft{verification:Some(cshell_domain::SftpVerification::Standard),verified:true,local_path:if cfg!(windows){"C:/stream.bin"}else{"/stream.bin"}.into(),remote_path:"/stream.bin".into(),..Default::default()},
            verified:Some(SftpVerifiedTransfer{digest_pending:true,content:Some(SftpVerifiedContent{size:MAX_SFTP_CHECKPOINT_BYTES*2,sha256:[0;32]}),temporary_owned:true,prefix_sha256:Some([1;32]),intent:SftpVerifiedIntent::Write,write_end:Some(MAX_SFTP_CHECKPOINT_BYTES),..Default::default()}),
        });
        queue.validate().unwrap();
        queue.items[0].verified.as_mut().unwrap().write_end=Some(MAX_SFTP_CHECKPOINT_BYTES+1);assert!(queue.validate().is_err());
        let v=queue.items[0].verified.as_mut().unwrap();v.write_end=None;v.intent=SftpVerifiedIntent::Publish;v.verified=true;v.bytes=v.content.unwrap().size;v.prefix_sha256=Some([0;32]);
        assert!(queue.validate().is_err()); // all ordinary completed-byte facts still cannot authorize a pending digest
        let v=queue.items[0].verified.as_mut().unwrap();v.digest_pending=false;queue.validate().unwrap();
    }
    #[test]
    fn recoverable_write_intents_accept_old_and_batched_boundaries_but_stay_bounded() {
        let mut queue = SftpQueueSnapshot::initial(SessionId::new());
        queue.items.push(SftpQueueItem {
            id: SftpQueueItemId::new(),
            state: SftpQueueState::Running,
            draft: SftpQueueDraft {
                verified: true,
                local_path: if cfg!(windows) {
                    "C:/transfer.bin".into()
                } else {
                    "/transfer.bin".into()
                },
                remote_path: "/transfer.bin".into(),
                ..Default::default()
            },
            transfer_id: Some(SessionId::new()),
            receipt: None,
            verified: Some(SftpVerifiedTransfer {
                content: Some(SftpVerifiedContent {
                    size: 2 * MAX_SFTP_WRITE_BATCH_BYTES,
                    sha256: [1; 32],
                }),
                temporary_owned: true,
                prefix_sha256: Some([2; 32]),
                intent: SftpVerifiedIntent::Write,
                write_end: Some(32 * 1024),
                ..Default::default()
            }),
        });
        queue.validate().unwrap();
        queue.items[0].verified.as_mut().unwrap().write_end = Some(MAX_SFTP_WRITE_BATCH_BYTES);
        queue.validate().unwrap();
        let encoded = serde_json::to_vec(&queue).unwrap();
        assert_eq!(decode_sftp_queue(&encoded).unwrap(), queue);
        for end in [0, MAX_SFTP_WRITE_BATCH_BYTES + 1, u64::MAX] {
            queue.items[0].verified.as_mut().unwrap().write_end = Some(end);
            assert!(queue.validate().is_err());
        }
    }
}

/// Explicit retry request; keeping the old journal permits checked prefix recovery.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SftpQueueRetry {
    pub items: Vec<SftpQueueItemId>,
    pub start: bool,
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    fn row() -> SftpQueueItem {
        SftpQueueItem {
            id: SftpQueueItemId::new(),
            state: SftpQueueState::Unknown,
            draft: SftpQueueDraft {
                verified: true,
                local_path: if cfg!(windows) {
                    "C:/retry.bin"
                } else {
                    "/retry.bin"
                }
                .into(),
                remote_path: "/retry.bin".into(),
                ..Default::default()
            },
            transfer_id: Some(SessionId::new()),
            receipt: None,
            verified: Some(SftpVerifiedTransfer::default()),
        }
    }
    #[test]
    fn retries_preserve_journals_and_reject_uncertain_create_or_publication() {
        let mut item = row();
        assert!(item.can_restart_verified());
        item.verified.as_mut().unwrap().content = Some(SftpVerifiedContent {
            size: 1024,
            sha256: [1; 32],
        });
        item.verified.as_mut().unwrap().intent = SftpVerifiedIntent::Create;
        assert!(!item.can_retry_verified());
        let v = item.verified.as_mut().unwrap();
        v.intent = SftpVerifiedIntent::Write;
        v.temporary_owned = true;
        v.prefix_sha256 = Some([2; 32]);
        v.write_end = Some(1024);
        assert!(item.can_resume_verified());
        assert!(!item.can_restart_verified());
        item.verified.as_mut().unwrap().intent = SftpVerifiedIntent::Publish;
        assert!(!item.can_retry_verified());
    }
    #[test]
    fn queued_retries_roundtrip_reject_missing_ids_and_parallel_destination_collisions() {
        let mut queue = SftpQueueSnapshot::initial(SessionId::new());
        queue.concurrency = Some(3);
        let item = row();
        queue.retry_items.insert(item.id);
        queue.items.push(item.clone());
        queue.validate().unwrap();
        assert_eq!(
            decode_sftp_queue(&serde_json::to_vec(&queue).unwrap()).unwrap(),
            queue
        );
        let mut duplicate = item.clone();
        duplicate.id = SftpQueueItemId::new();
        duplicate.transfer_id = Some(SessionId::new());
        queue.retry_items.insert(duplicate.id);
        queue.items.push(duplicate);
        assert!(queue.validate().is_err());
        queue.items.pop();
        queue.retry_items.remove(&queue.items[0].id);
        assert!(queue.validate().is_err());
        queue.retry_items.clear();
        let encoded = serde_json::to_vec(&queue).unwrap();
        assert!(
            !String::from_utf8(encoded.clone())
                .unwrap()
                .contains("retry_items")
        );
        decode_sftp_queue(&encoded).unwrap();
    }
}
