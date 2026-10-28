use super::queue::Queues;
use super::*;
use cshell_application::*;
use sha2::{Digest, Sha256};
impl SftpIpcService {
    pub(super) async fn migrate_queue(
        &self,
        request: SftpRequest,
        session: SessionId,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        let result = self.migrate_inner(&request, session, sessions).await;
        match result {
            Ok(q) => SftpResponse {
                document_json: serde_json::to_vec(&SftpWorkspaceReply::Queue(q))
                    .unwrap_or_default(),
                ..Default::default()
            },
            Err(status) => SftpResponse::with_status(
                status,
                "Queue migration refused or uncertain; reload both queues before continuing",
            ),
        }
    }
    async fn migrate_inner(
        &self,
        request: &SftpRequest,
        session: SessionId,
        sessions: &SshSessionRegistry,
    ) -> Result<SftpQueueSnapshot, SftpStatus> {
        let canonical = SftpRequest {
            operation: SftpOperation::QueueMigrate as i32,
            session_id: request.session_id.clone(),
            document_json: request.document_json.clone(),
            expected_revision: request.expected_revision,
            confirmed: true,
            ..Default::default()
        };
        if canonical != *request
            || request.document_json.len() > 16 * 1024
            || request.expected_revision == 0
        {
            return Err(SftpStatus::InvalidRequest);
        }
        let plan: SftpQueueMigration = serde_json::from_slice(&request.document_json)
            .map_err(|_| SftpStatus::InvalidRequest)?;
        if session == plan.target || plan.target.as_uuid().is_nil() || plan.target_revision == 0 {
            return Err(SftpStatus::InvalidRequest);
        }
        let first = self.queues.lock(session.min(plan.target))?;
        let second = self.queues.lock(session.max(plan.target))?;
        let _first = first.lock().await;
        let _second = second.lock().await;
        if self.queues.active(session)
            || self.queues.active(plan.target)
            || self
                .jobs
                .lock()
                .map_err(|_| SftpStatus::Unavailable)?
                .values()
                .any(|j| {
                    (j.session_id == session || j.session_id == plan.target)
                        && j.snapshot.state == SftpTransferState::Running as i32
                })
        {
            return Err(SftpStatus::Conflict);
        }
        let live = sessions
            .get_sftp(plan.target)
            .map_err(|_| SftpStatus::Unavailable)?;
        if !live.running() || live.clipboard_trust() != Some(&plan.peer) {
            return Err(SftpStatus::Conflict);
        }
        let mut source = Queues::load_async(session).await?;
        if source.revision != request.expected_revision
            || !source.paused
            || source
                .items
                .iter()
                .any(|i| i.state == SftpQueueState::Running)
        {
            return Err(SftpStatus::Conflict);
        }
        if source.peer.is_none() {
            source.peer = sessions
                .get_sftp(session)
                .ok()
                .and_then(|s| s.clipboard_trust().cloned());
        }
        if source.peer.as_ref() != Some(&plan.peer) {
            return Err(SftpStatus::Conflict);
        }
        // Hash only the immutable import payload; source revision advances when sealing it.
        let digest: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&(&source.peer, &source.concurrency, &source.items))
                .map_err(|_| SftpStatus::InvalidRequest)?,
        )
        .into();
        if source.migrated_to.is_some_and(|t| t != plan.target)
            || source.migration_digest.is_some_and(|d| d != digest)
        {
            return Err(SftpStatus::Conflict);
        }
        let mut target = Queues::load_async(plan.target).await?;
        if target
            .migration_origin
            .as_ref()
            .is_some_and(|o| o.session == session && o.digest == digest)
        {
            return Ok(target);
        }
        if target.revision != plan.target_revision
            || !target.paused
            || !target.items.is_empty()
            || target.migrated_to.is_some()
            || target.migration_origin.is_some()
            || target.peer.as_ref().is_some_and(|p| p != &plan.peer)
        {
            return Err(SftpStatus::Conflict);
        }
        if source.migrated_to.is_none() {
            source.migrated_to = Some(plan.target);
            source.migration_digest = Some(digest);
            source.changed().map_err(|_| SftpStatus::InvalidRequest)?;
            Queues::save_async(&source).await?;
        }
        // A failed target write leaves the source sealed. An explicit identical retry
        // can finish this metadata import; it never starts or repeats a file operation.
        target.peer = Some(plan.peer);
        target.concurrency = source.concurrency;
        target.items = source.items;
        target.migration_origin = Some(SftpMigrationOrigin { session, digest });
        target.paused = true;
        target.changed().map_err(|_| SftpStatus::InvalidRequest)?;
        Queues::save_async(&target).await?;
        Ok(target)
    }
}
