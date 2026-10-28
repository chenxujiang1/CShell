use super::*;
use cshell_application::*;
use cshell_domain::SftpQueueItemId;
use cshell_ipc::RuntimePaths;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::{Read, Write},
    path::Path,
};
#[derive(Clone, Debug, Default)]
pub(super) struct Queues {
    locks: Arc<Mutex<BTreeMap<SessionId, Arc<tokio::sync::Mutex<()>>>>>,
    active: Arc<Mutex<BTreeSet<SessionId>>>,
    current: Arc<Mutex<BTreeMap<SessionId, BTreeSet<Vec<u8>>>>>,
}
struct Owner {
    active: Arc<Mutex<BTreeSet<SessionId>>>,
    session: SessionId,
    current: Arc<Mutex<BTreeMap<SessionId, BTreeSet<Vec<u8>>>>>,
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session);
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.session);
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    format: String,
    version: u32,
    sha256: [u8; 32],
    snapshot: SftpQueueSnapshot,
}
impl Queues {
    pub(super) fn owns_transfer(&self, session: SessionId, id: &[u8]) -> bool {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session)
            .is_some_and(|current| current.contains(id))
    }
    pub(super) fn lock(
        &self,
        session: SessionId,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, SftpStatus> {
        let mut locks = self.locks.lock().map_err(|_| SftpStatus::Unavailable)?;
        if locks.len() >= 512 {
            locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        if locks.len() >= 512 && !locks.contains_key(&session) {
            return Err(SftpStatus::CapacityReached);
        }
        Ok(locks.entry(session).or_default().clone())
    }
    pub(super) fn active(&self, session: SessionId) -> bool {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&session)
    }
    pub(super) fn root() -> Result<PathBuf, SftpStatus> {
        let paths = RuntimePaths::for_current_user().map_err(|_| SftpStatus::Unavailable)?;
        Ok(RuntimePaths::prepare(paths.root().join("sftp-queues"))
            .map_err(|_| SftpStatus::Unavailable)?
            .root()
            .to_owned())
    }
    fn load(session: SessionId) -> Result<SftpQueueSnapshot, SftpStatus> {
        let path = Self::root()?.join(format!("{session}.json"));
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SftpQueueSnapshot::initial(session));
            }
            Err(_) => return Err(SftpStatus::Unavailable),
        };
        if !meta.is_file()
            || meta.file_type().is_symlink()
            || meta.len() > (MAX_SFTP_QUEUE_DOCUMENT + 1024) as u64
        {
            return Err(SftpStatus::InvalidRequest);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(SftpStatus::InvalidRequest);
            }
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000);
        }
        let mut file = options.open(path).map_err(|_| SftpStatus::Unavailable)?;
        let mut bytes = Vec::new();
        (&mut file)
            .take((MAX_SFTP_QUEUE_DOCUMENT + 1025) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| SftpStatus::Unavailable)?;
        let stored: Stored =
            serde_json::from_slice(&bytes).map_err(|_| SftpStatus::InvalidRequest)?;
        stored
            .snapshot
            .validate()
            .map_err(|_| SftpStatus::InvalidRequest)?;
        let digest: [u8; 32] = Sha256::digest(
            serde_json::to_vec(&stored.snapshot).map_err(|_| SftpStatus::InvalidRequest)?,
        )
        .into();
        if stored.format != "cshell.sftp-queue-storage"
            || stored.version != 1
            || stored.sha256 != digest
            || stored.snapshot.session_id != session
        {
            return Err(SftpStatus::InvalidRequest);
        }
        Ok(stored.snapshot)
    }
    fn save(snapshot: &SftpQueueSnapshot) -> Result<(), SftpStatus> {
        snapshot
            .validate()
            .map_err(|_| SftpStatus::InvalidRequest)?;
        let bytes = serde_json::to_vec(snapshot).map_err(|_| SftpStatus::InvalidRequest)?;
        let stored = Stored {
            format: "cshell.sftp-queue-storage".into(),
            version: 1,
            sha256: Sha256::digest(&bytes).into(),
            snapshot: snapshot.clone(),
        };
        let root = Self::root()?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x0020_0000);
        }
        let lease = options
            .open(root.join(format!("{}.lock", snapshot.session_id)))
            .map_err(|_| SftpStatus::Unavailable)?;
        let meta = lease.metadata().map_err(|_| SftpStatus::Unavailable)?;
        if !meta.is_file() {
            return Err(SftpStatus::InvalidRequest);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                return Err(SftpStatus::InvalidRequest);
            }
        }
        lease.try_lock().map_err(|_| SftpStatus::Conflict)?;
        let actual = Self::load(snapshot.session_id)?;
        if actual.revision.checked_add(1) != Some(snapshot.revision) {
            return Err(SftpStatus::Conflict);
        }
        let mut file =
            tempfile::NamedTempFile::new_in(&root).map_err(|_| SftpStatus::Unavailable)?;
        serde_json::to_writer(&mut file, &stored).map_err(|_| SftpStatus::Unavailable)?;
        file.as_file()
            .sync_all()
            .map_err(|_| SftpStatus::Unavailable)?;
        file.persist(root.join(format!("{}.json", snapshot.session_id)))
            .map_err(|_| SftpStatus::Unavailable)?;
        #[cfg(unix)]
        std::fs::File::open(root)
            .and_then(|f| f.sync_all())
            .map_err(|_| SftpStatus::Unavailable)?;
        Ok(())
    }
    pub(super) async fn load_async(session: SessionId) -> Result<SftpQueueSnapshot, SftpStatus> {
        tokio::task::spawn_blocking(move || Self::load(session))
            .await
            .map_err(|_| SftpStatus::Unavailable)?
    }
    pub(super) async fn save_async(snapshot: &SftpQueueSnapshot) -> Result<(), SftpStatus> {
        let snapshot = snapshot.clone();
        tokio::task::spawn_blocking(move || Self::save(&snapshot))
            .await
            .map_err(|_| SftpStatus::Unavailable)?
    }
    /// Keep the session lock alive until a blocking journal write actually finishes,
    /// even if transfer cancellation drops the awaiting future.
    pub(super) async fn save_owned(
        snapshot: &SftpQueueSnapshot,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<(), SftpStatus> {
        let snapshot = snapshot.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = guard;
            Self::save(&snapshot)
        })
        .await
        .map_err(|_| SftpStatus::Unavailable)?
    }
    pub(super) async fn recover(&self, snapshot: &mut SftpQueueSnapshot) -> Result<(), SftpStatus> {
        if self.active(snapshot.session_id) {
            return Ok(());
        }
        let mut changed = !snapshot.paused;
        for item in &mut snapshot.items {
            if item.state == SftpQueueState::Running {
                item.state = SftpQueueState::Unknown;
                changed = true;
            }
        }
        if changed {
            snapshot.paused = true;
            snapshot.changed().map_err(|_| SftpStatus::InvalidRequest)?;
            Self::save_async(snapshot).await?;
        }
        Ok(())
    }
}
impl SftpIpcService {
    pub(super) async fn queue(
        &self,
        request: SftpRequest,
        session: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
    ) -> SftpResponse {
        match self
            .queue_inner(&request, session, operation, sessions, None)
            .await
        {
            Ok(snapshot) => SftpResponse {
                document_json: serde_json::to_vec(&snapshot).unwrap_or_default(),
                ..Default::default()
            },
            Err(status) => SftpResponse::with_status(
                status,
                "Queue request rejected; reload saved queue before another mutation",
            ),
        }
    }
    async fn queue_inner(
        &self,
        request: &SftpRequest,
        session: SessionId,
        operation: SftpOperation,
        sessions: &SshSessionRegistry,
        guard: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<SftpQueueSnapshot, SftpStatus> {
        let mut canonical = SftpRequest {
            operation: request.operation,
            session_id: request.session_id.clone(),
            ..Default::default()
        };
        if operation != SftpOperation::QueueGet {
            canonical.confirmed = request.confirmed;
            canonical.expected_revision = request.expected_revision;
            if !request.confirmed || request.expected_revision == 0 {
                return Err(SftpStatus::InvalidRequest);
            }
        }
        if matches!(
            operation,
            SftpOperation::QueueAdd
                | SftpOperation::QueueAddVerified
                | SftpOperation::QueueAddManaged
                | SftpOperation::QueueEdit
                | SftpOperation::QueueConfigure
                | SftpOperation::QueueRetryMany
        ) {
            canonical.document_json.clone_from(&request.document_json);
        }
        if matches!(
            operation,
            SftpOperation::QueueCancelPending
                | SftpOperation::QueueRemove
                | SftpOperation::QueueResumeVerified
                | SftpOperation::QueueAdoptVerified
                | SftpOperation::QueueCleanupVerified
                | SftpOperation::QueueEdit
                | SftpOperation::QueueStartOne
        ) {
            canonical.transfer_id.clone_from(&request.transfer_id);
        }
        if canonical != *request || session.as_uuid().is_nil() {
            return Err(SftpStatus::InvalidRequest);
        }
        let _guard = match guard {
            Some(guard) => guard,
            None => self.queues.lock(session)?.lock_owned().await,
        };
        let mut snapshot = Queues::load_async(session).await?;
        self.queues.recover(&mut snapshot).await?;
        if operation == SftpOperation::QueueGet {
            return Ok(snapshot);
        }
        if snapshot.migrated_to.is_some() {
            return Err(SftpStatus::Conflict);
        }
        if let Some(origin) = &snapshot.migration_origin {
            let source = Queues::load_async(origin.session).await?;
            if source.migrated_to != Some(session) || source.migration_digest != Some(origin.digest)
            {
                return Err(SftpStatus::Conflict);
            }
        }
        if snapshot.revision != request.expected_revision {
            return Err(SftpStatus::StaleRevision);
        }
        match operation {
            SftpOperation::QueueAdd
            | SftpOperation::QueueAddVerified
            | SftpOperation::QueueAddManaged
            | SftpOperation::QueueEdit => {
                if request.document_json.len() > 16 * 1024
                    || snapshot.items.len() >= MAX_SFTP_QUEUE_ITEMS
                {
                    return Err(SftpStatus::CapacityReached);
                }
                let draft: SftpQueueDraft = serde_json::from_slice(&request.document_json)
                    .map_err(|_| SftpStatus::InvalidRequest)?;
                draft.validate().map_err(|_| SftpStatus::InvalidRequest)?;
                if draft.verified != (operation != SftpOperation::QueueAdd)
                    || draft.managed()
                        && !matches!(
                            operation,
                            SftpOperation::QueueAddManaged | SftpOperation::QueueEdit
                        )
                {
                    return Err(SftpStatus::InvalidRequest);
                }
                let live = sessions.get_sftp(session).map_err(|_| SftpStatus::Unavailable)?;
                let peer = live.clipboard_trust().cloned();
                if snapshot.peer.is_some() && snapshot.peer != peer {
                    return Err(SftpStatus::Conflict);
                }
                if snapshot.peer.is_none()
                    && matches!(
                        operation,
                        SftpOperation::QueueAddManaged | SftpOperation::QueueEdit
                    )
                {
                    if peer.is_none() {
                        return Err(SftpStatus::Unavailable);
                    }
                    snapshot.peer = peer;
                }
                if operation == SftpOperation::QueueEdit {
                    if !snapshot.paused || self.queues.active(session) {
                        return Err(SftpStatus::Conflict);
                    }
                    let id = SftpQueueItemId::from_bytes(
                        request
                            .transfer_id
                            .as_slice()
                            .try_into()
                            .map_err(|_| SftpStatus::InvalidRequest)?,
                    );
                    let row = snapshot
                        .items
                        .iter_mut()
                        .find(|i| i.id == id && i.state == SftpQueueState::Pending)
                        .ok_or(SftpStatus::Conflict)?;
                    if row.draft.expected_source.is_some()
                        || row.draft.expected_destination.is_some()
                    {
                        return Err(SftpStatus::Conflict);
                    }
                    row.draft = draft;
                } else {
                    if operation == SftpOperation::QueueAddManaged {
                        snapshot.paused = true;
                    }
                    snapshot.items.push(SftpQueueItem {
                        verified: None,
                        id: SftpQueueItemId::new(),
                        draft,
                        state: SftpQueueState::Pending,
                        transfer_id: None,
                        receipt: None,
                    });
                }
            }
            SftpOperation::QueueConfigure => {
                let live = sessions.get_sftp(session).map_err(|_| SftpStatus::Unavailable)?;
                let peer = live
                    .clipboard_trust()
                    .cloned()
                    .ok_or(SftpStatus::Unavailable)?;
                if snapshot.peer.as_ref().is_some_and(|p| p != &peer) {
                    return Err(SftpStatus::Conflict);
                }
                snapshot.peer = Some(peer);
                if !snapshot.paused
                    || self.queues.active(session)
                    || request.document_json.len() > 1024
                {
                    return Err(SftpStatus::Conflict);
                }
                let settings: SftpQueueSettings = serde_json::from_slice(&request.document_json)
                    .map_err(|_| SftpStatus::InvalidRequest)?;
                if !(1..=4).contains(&settings.concurrency) {
                    return Err(SftpStatus::InvalidRequest);
                }
                snapshot.concurrency = Some(settings.concurrency);
            }
            SftpOperation::QueueRetryMany => {
                if request.document_json.len() > 8192 {
                    return Err(SftpStatus::InvalidRequest);
                }
                let plan: SftpQueueRetry = serde_json::from_slice(&request.document_json)
                    .map_err(|_| SftpStatus::InvalidRequest)?;
                let ids: BTreeSet<_> = plan.items.iter().copied().collect();
                if ids.is_empty()
                    || ids.len() != plan.items.len()
                    || ids.len() > MAX_SFTP_QUEUE_ITEMS
                    || ids.iter().any(|id| {
                        snapshot.retry_items.contains(id)
                            || !snapshot
                                .items
                                .iter()
                                .any(|item| item.id == *id && item.can_retry_verified())
                    })
                {
                    return Err(SftpStatus::Conflict);
                }
                let target = sessions.get_sftp(session).map_err(|_| SftpStatus::Unavailable)?;
                if !target.running() {
                    return Err(SftpStatus::Unavailable);
                }
                let peer = target
                    .clipboard_trust()
                    .ok_or(SftpStatus::Unavailable)?
                    .clone();
                if snapshot
                    .peer
                    .as_ref()
                    .is_some_and(|previous| previous != &peer)
                {
                    return Err(SftpStatus::Conflict);
                }
                snapshot.peer = Some(peer);
                snapshot.retry_items.extend(ids);
                if plan.start {
                    snapshot.paused = false;
                }
                snapshot.changed().map_err(|_| SftpStatus::Conflict)?;
                Queues::save_async(&snapshot).await?;
                if plan.start {
                    self.ensure_queue_runner(session, target)?;
                }
                return Ok(snapshot);
            }
            SftpOperation::QueuePause => {
                snapshot.paused = true;
            }
            SftpOperation::QueueCancelPending => {
                let item = if request.transfer_id.is_empty() {
                    None
                } else {
                    Some(SftpQueueItemId::from_bytes(
                        request
                            .transfer_id
                            .as_slice()
                            .try_into()
                            .map_err(|_| SftpStatus::InvalidRequest)?,
                    ))
                };
                let mut changed = false;
                for row in &mut snapshot.items {
                    if row.state == SftpQueueState::Pending && item.is_none_or(|id| id == row.id) {
                        row.state = SftpQueueState::Cancelled;
                        changed = true;
                    }
                }
                if !changed
                    && !snapshot
                        .retry_items
                        .iter()
                        .any(|id| item.is_none_or(|selected| selected == *id))
                {
                    return Err(SftpStatus::Conflict);
                }
                let retries: Vec<_> = snapshot
                    .retry_items
                    .iter()
                    .copied()
                    .filter(|id| item.is_none_or(|selected| selected == *id))
                    .collect();
                for id in retries {
                    snapshot.retry_items.remove(&id);
                }
                if item.is_none() {
                    snapshot.paused = true;
                }
            }
            SftpOperation::QueueRemove => {
                let id = SftpQueueItemId::from_bytes(
                    request
                        .transfer_id
                        .as_slice()
                        .try_into()
                        .map_err(|_| SftpStatus::InvalidRequest)?,
                );
                if !snapshot
                    .items
                    .iter()
                    .any(|i| i.id == id && i.state != SftpQueueState::Running)
                {
                    return Err(SftpStatus::Conflict);
                }
                snapshot.items.retain(|i| i.id != id);
                snapshot.retry_items.remove(&id);
            }
            SftpOperation::QueueStart
            | SftpOperation::QueueStartVerified
            | SftpOperation::QueueStartManaged => {
                if operation != SftpOperation::QueueStartManaged
                    && (snapshot.concurrency.unwrap_or(1) > 1
                        || snapshot
                            .items
                            .iter()
                            .any(|i| i.state == SftpQueueState::Pending && i.draft.managed()))
                {
                    return Err(SftpStatus::Unsupported);
                }
                if operation == SftpOperation::QueueStart
                    && snapshot
                        .items
                        .iter()
                        .any(|i| i.state == SftpQueueState::Pending && i.draft.verified)
                {
                    return Err(SftpStatus::Unsupported);
                }
                if snapshot.retry_items.is_empty()
                    && !snapshot.items.iter().any(|i| {
                        matches!(i.state, SftpQueueState::Pending | SftpQueueState::Running)
                    })
                {
                    return Err(SftpStatus::Conflict);
                }
                let target = sessions.get_sftp(session).map_err(|_| SftpStatus::Unavailable)?;
                if !target.running() {
                    return Err(SftpStatus::Unavailable);
                }
                if operation != SftpOperation::QueueStartManaged && self.queues.active(session) {
                    return Err(SftpStatus::Conflict);
                }
                snapshot.paused = false;
                snapshot.changed().map_err(|_| SftpStatus::InvalidRequest)?;
                Queues::save_async(&snapshot).await?;
                self.pause_session_transfers(session, false);
                self.ensure_queue_runner(session, target)?;
                return Ok(snapshot);
            }
            SftpOperation::QueueResumeVerified
            | SftpOperation::QueueAdoptVerified
            | SftpOperation::QueueCleanupVerified
            | SftpOperation::QueueStartOne => {
                if self.queues.active(session) || !snapshot.paused {
                    return Err(SftpStatus::Conflict);
                }
                let id = SftpQueueItemId::from_bytes(
                    request
                        .transfer_id
                        .as_slice()
                        .try_into()
                        .map_err(|_| SftpStatus::InvalidRequest)?,
                );
                let previous = snapshot
                    .items
                    .iter()
                    .find(|i| i.id == id)
                    .cloned()
                    .ok_or(SftpStatus::Conflict)?;
                let allowed = match operation {
                    SftpOperation::QueueStartOne => {
                        previous.state == SftpQueueState::Pending && previous.draft.verified
                    }
                    SftpOperation::QueueResumeVerified => previous.can_resume_verified(),
                    SftpOperation::QueueAdoptVerified => previous.can_adopt_verified(),
                    SftpOperation::QueueCleanupVerified => previous.can_cleanup_verified(),
                    _ => false,
                };
                if !allowed {
                    return Err(SftpStatus::Conflict);
                }
                let target = sessions.get_sftp(session).map_err(|_| SftpStatus::Unavailable)?;
                if !target.running() {
                    return Err(SftpStatus::Unavailable);
                }
                {
                    let mut active = self
                        .queues
                        .active
                        .lock()
                        .map_err(|_| SftpStatus::Unavailable)?;
                    if active.len() >= MAX_ACTIVE_TRANSFERS {
                        return Err(SftpStatus::CapacityReached);
                    }
                    active.insert(session);
                }
                let owner = Owner {
                    current: self.queues.current.clone(),
                    active: self.queues.active.clone(),
                    session,
                };
                let transfer = SessionId::new();
                let item = snapshot
                    .items
                    .iter_mut()
                    .find(|i| i.id == id)
                    .ok_or(SftpStatus::Conflict)?;
                item.state = SftpQueueState::Running;
                if operation == SftpOperation::QueueStartOne {
                    item.verified = Some(SftpVerifiedTransfer::default());
                }
                item.transfer_id = Some(transfer);
                item.receipt = None;
                snapshot.retry_items.remove(&id);
                snapshot.changed().map_err(|_| SftpStatus::InvalidRequest)?;
                Queues::save_async(&snapshot).await?;
                self.queues
                    .current
                    .lock()
                    .map_err(|_| SftpStatus::Unavailable)?
                    .entry(session)
                    .or_default()
                    .insert(transfer.as_uuid().as_bytes().to_vec());
                let service = self.clone();
                tokio::spawn(async move {
                    let _owner = owner;
                    let response = service.start_verified(
                        session,
                        target.clone(),
                        id,
                        transfer,
                        verified::VerifiedAction::from_operation(operation),
                        1,
                    );
                    if response.status != SftpStatus::Ok as i32 {
                        let lock = service.queues.lock(session)?;
                        let _guard = lock.lock().await;
                        let mut saved = Queues::load_async(session).await?;
                        let row = saved
                            .items
                            .iter_mut()
                            .find(|i| {
                                i.id == id
                                    && i.transfer_id == Some(transfer)
                                    && i.state == SftpQueueState::Running
                            })
                            .ok_or(SftpStatus::Conflict)?;
                        *row = previous;
                        saved.paused = true;
                        saved.changed().map_err(|_| SftpStatus::InvalidRequest)?;
                        Queues::save_async(&saved).await?;
                    } else {
                        service.wait_queue_transfer(session, id, transfer).await?;
                    }
                    drop(_owner);
                    let saved = Queues::load_async(session).await?;
                    if !saved.paused
                        && saved
                            .items
                            .iter()
                            .any(|item| item.state == SftpQueueState::Pending)
                    {
                        service.ensure_queue_runner(session, target)?;
                    }
                    Ok::<(), SftpStatus>(())
                });
                return Ok(snapshot);
            }
            _ => return Err(SftpStatus::InvalidRequest),
        }
        snapshot.changed().map_err(|_| SftpStatus::InvalidRequest)?;
        Queues::save_async(&snapshot).await?;
        if operation == SftpOperation::QueuePause {
            self.pause_session_transfers(session, true);
        }
        Ok(snapshot)
    }
    // A terminal drop owns its private queue. Read its revision and apply a
    // control under the same lock so progress checkpoints cannot starve pause
    // or cancellation by invalidating a separate read/mutate retry loop.
    pub(super) async fn mutate_upload_queue(
        &self,
        sessions: &SshSessionRegistry,
        session: SessionId,
        operation: SftpOperation,
        item: Vec<u8>,
    ) -> Result<(), SftpStatus> {
        if !matches!(
            operation,
            SftpOperation::QueuePause
                | SftpOperation::QueueStartManaged
                | SftpOperation::QueueCancelPending
                | SftpOperation::QueueRemove
        ) {
            return Err(SftpStatus::InvalidRequest);
        }
        let guard = self.queues.lock(session)?.lock_owned().await;
        let mut before = Queues::load_async(session).await?;
        self.queues.recover(&mut before).await?;
        let request = SftpRequest {
            operation: operation as i32,
            session_id: session.as_uuid().as_bytes().to_vec(),
            transfer_id: item,
            expected_revision: before.revision,
            confirmed: true,
            ..Default::default()
        };
        self.queue_inner(&request, session, operation, sessions, Some(guard))
            .await?;
        Ok(())
    }
    pub(super) fn pause_session_transfers(&self, session: SessionId, paused: bool) {
        let jobs = self
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for job in jobs.values().filter(|job| {
            job.session_id == session && job.snapshot.state == SftpTransferState::Running as i32
        }) {
            if paused {
                job.control.pause();
            } else {
                job.control.resume();
            }
        }
    }
    pub(super) fn ensure_queue_runner(
        &self,
        session: SessionId,
        target: Arc<SshSession>,
    ) -> Result<(), SftpStatus> {
        {
            let mut active = self
                .queues
                .active
                .lock()
                .map_err(|_| SftpStatus::Unavailable)?;
            if active.contains(&session) {
                return Ok(());
            }
            if active.len() >= MAX_ACTIVE_TRANSFERS {
                return Err(SftpStatus::CapacityReached);
            }
            active.insert(session);
        }
        let owner = Owner {
            current: self.queues.current.clone(),
            active: self.queues.active.clone(),
            session,
        };
        let service = self.clone();
        tokio::spawn(async move {
            let _ = service.queue_runner(session, target.clone()).await;
            drop(owner);
            // An explicit start/retry can arrive between the runner's final unlock
            // and release of its ownership lease. Recheck after dropping the lease.
            if let Ok(lock) = service.queues.lock(session) {
                let _guard = lock.lock().await;
                if let Ok(saved) = Queues::load_async(session).await {
                    if !saved.paused
                        && (!saved.retry_items.is_empty()
                            || saved
                                .items
                                .iter()
                                .any(|item| item.state == SftpQueueState::Pending))
                    {
                        let _ = service.ensure_queue_runner(session, target);
                    }
                }
            }
        });
        Ok(())
    }
    async fn queue_runner(
        &self,
        session: SessionId,
        target: Arc<SshSession>,
    ) -> Result<(), SftpStatus> {
        let lock = self.queues.lock(session)?;
        let mut workers = tokio::task::JoinSet::new();
        let mut failed = false;
        loop {
            {
                let _guard = lock.lock().await;
                let mut snapshot = Queues::load_async(session).await?;
                if !snapshot.paused {
                    let concurrency = snapshot.concurrency.unwrap_or(1) as usize;
                    while workers.len() < concurrency {
                        let retries = snapshot.retry_items.clone();
                        let Some(item) = snapshot.items.iter_mut().find(|i| {
                            i.state == SftpQueueState::Pending || retries.contains(&i.id)
                        }) else {
                            break;
                        };
                        let previous = item.clone();
                        let retry = retries.contains(&item.id);
                        let resume = retry && item.can_resume_verified();
                        let id = item.id;
                        let draft = item.draft.clone();
                        let transfer = SessionId::new();
                        item.state = SftpQueueState::Running;
                        item.transfer_id = Some(transfer);
                        item.receipt = None;
                        if draft.verified && !resume {
                            item.verified = Some(SftpVerifiedTransfer::default());
                        }
                        snapshot.retry_items.remove(&id);
                        snapshot.changed().map_err(|_| SftpStatus::InvalidRequest)?;
                        Queues::save_async(&snapshot).await?;
                        self.queues
                            .current
                            .lock()
                            .map_err(|_| SftpStatus::Unavailable)?
                            .entry(session)
                            .or_default()
                            .insert(transfer.as_uuid().as_bytes().to_vec());
                        let response = if draft.verified {
                            self.start_verified(
                                session,
                                target.clone(),
                                id,
                                transfer,
                                verified::VerifiedAction::Transfer { resume },
                                concurrency,
                            )
                        } else {
                            self.start_bound(
                                SftpRequest {
                                    operation: if draft.direction == SftpQueueDirection::Upload {
                                        SftpOperation::Upload
                                    } else {
                                        SftpOperation::Download
                                    } as i32,
                                    session_id: session.as_uuid().as_bytes().to_vec(),
                                    remote_path: draft.remote_path,
                                    local_path: draft.local_path,
                                    ..Default::default()
                                },
                                if draft.direction == SftpQueueDirection::Upload {
                                    SftpOperation::Upload
                                } else {
                                    SftpOperation::Download
                                },
                                session,
                                target.clone(),
                                Some(transfer.as_uuid().as_bytes().to_vec()),
                            )
                        };
                        if response.status != SftpStatus::Ok as i32 {
                            let item = snapshot
                                .items
                                .iter_mut()
                                .find(|i| i.id == id)
                                .ok_or(SftpStatus::Conflict)?;
                            *item = previous;
                            if retry {
                                snapshot.retry_items.insert(id);
                            }
                            snapshot.paused = true;
                            snapshot.changed().map_err(|_| SftpStatus::InvalidRequest)?;
                            Queues::save_async(&snapshot).await?;
                            self.queues
                                .current
                                .lock()
                                .map_err(|_| SftpStatus::Unavailable)?
                                .entry(session)
                                .or_default()
                                .remove(transfer.as_uuid().as_bytes().as_slice());
                            break;
                        }
                        let service = self.clone();
                        workers.spawn(async move {
                            service.wait_queue_transfer(session, id, transfer).await
                        });
                    }
                }
                if workers.is_empty() {
                    if !snapshot.paused {
                        snapshot.paused = true;
                        snapshot.changed().map_err(|_| SftpStatus::InvalidRequest)?;
                        Queues::save_async(&snapshot).await?;
                    }
                    return if failed {
                        Err(SftpStatus::Unavailable)
                    } else {
                        Ok(())
                    };
                }
            }
            tokio::select! {
                result = workers.join_next() => { if !matches!(result, Some(Ok(Ok(())))) { failed = true; } }
                _ = tokio::time::sleep(std::time::Duration::from_millis(250)) => {}
            }
        }
    }
    async fn wait_queue_transfer(
        &self,
        session: SessionId,
        id: SftpQueueItemId,
        transfer_id: SessionId,
    ) -> Result<(), SftpStatus> {
        let lock = self.queues.lock(session)?;
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            let transfer = self
                .jobs
                .lock()
                .map_err(|_| SftpStatus::Unavailable)?
                .get(transfer_id.as_uuid().as_bytes().as_slice())
                .map(|j| j.snapshot.clone())
                .ok_or(SftpStatus::Unavailable)?;
            if transfer.state == SftpTransferState::Running as i32 {
                continue;
            }
            let _guard = lock.lock().await;
            let mut snapshot = Queues::load_async(session).await?;
            let item = snapshot
                .items
                .iter_mut()
                .find(|i| {
                    i.id == id
                        && i.transfer_id == Some(transfer_id)
                        && i.state == SftpQueueState::Running
                })
                .ok_or(SftpStatus::InvalidRequest)?;
            let mut transfer = transfer;
            if let Some(v) = &item.verified {
                // A cancelled blocking save can finish before this fresh load.
                // The durable journal, rather than an earlier memory view, owns the facts.
                transfer.bytes_transferred = v.bytes;
                transfer.total_bytes = v.content.map(|c| c.size);
                transfer.destination_committed = v.committed;
                transfer.cleanup_path = if v.temporary_owned && !v.cleaned {
                    item.temporary_path().ok()
                } else {
                    None
                };
                transfer.cleanup_location = if transfer.cleanup_path.is_none() {
                    SftpCleanupLocation::Unspecified as i32
                } else if item.draft.direction == SftpQueueDirection::Upload {
                    SftpCleanupLocation::Remote as i32
                } else {
                    SftpCleanupLocation::DaemonLocal as i32
                };
                transfer.state = if v.committed && v.cleaned {
                    transfer.detail.clear();
                    SftpTransferState::Succeeded as i32
                } else {
                    SftpTransferState::Failed as i32
                };
            }
            item.state = if transfer.state == SftpTransferState::Succeeded as i32 {
                SftpQueueState::Succeeded
            } else if transfer.destination_committed {
                SftpQueueState::CleanupRequired
            } else {
                SftpQueueState::Unknown
            };
            item.receipt = Some(SftpQueueReceipt {
                detail: (!transfer.detail.is_empty()).then_some(transfer.detail.clone()),
                bytes: transfer.bytes_transferred,
                total: transfer.total_bytes,
                destination_committed: transfer.destination_committed,
                cleanup_remote: transfer.cleanup_location == SftpCleanupLocation::Remote as i32,
                cleanup_path: transfer.cleanup_path.clone(),
            });
            if item.state != SftpQueueState::Succeeded {
                snapshot.paused = true;
            }
            snapshot.changed().map_err(|_| SftpStatus::InvalidRequest)?;
            Queues::save_async(&snapshot).await?;
            self.queues
                .current
                .lock()
                .map_err(|_| SftpStatus::Unavailable)?
                .entry(session)
                .or_default()
                .remove(transfer_id.as_uuid().as_bytes().as_slice());
            return Ok(());
        }
    }
    pub(super) async fn local_list(&self, request: SftpRequest) -> SftpResponse {
        let canonical = SftpRequest {
            operation: request.operation,
            session_id: request.session_id.clone(),
            local_path: request.local_path.clone(),
            max_entries: request.max_entries,
            ..Default::default()
        };
        if canonical != request
            || !(1..=MAX_SFTP_DIRECTORY_ENTRIES).contains(&request.max_entries)
            || !request.local_path.is_empty()
                && (!valid_path(&request.local_path)
                    || request.local_path.chars().any(char::is_control))
        {
            return SftpResponse::with_status(
                SftpStatus::InvalidRequest,
                "Invalid local list request",
            );
        }
        let result = tokio::task::spawn_blocking(move || {
            let path = if request.local_path.is_empty() {
                #[cfg(windows)]
                let home = std::env::var_os("USERPROFILE");
                #[cfg(not(windows))]
                let home = std::env::var_os("HOME");
                PathBuf::from(home.ok_or("Home directory unavailable")?)
            } else {
                PathBuf::from(request.local_path)
            };
            if !path.is_absolute() {
                return Err("Local directory must be absolute");
            }
            for ancestor in path.ancestors() {
                let meta = std::fs::symlink_metadata(ancestor)
                    .map_err(|_| "Local directory unavailable")?;
                if !meta.is_dir() || meta.file_type().is_symlink() {
                    return Err("Directory links are not followed");
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        return Err("Directory reparse points are not followed");
                    }
                }
            }
            let mut entries = Vec::new();
            let mut truncated = false;
            for entry in std::fs::read_dir(&path).map_err(|_| "Local directory unavailable")? {
                let entry = entry.map_err(|_| "Local entry unavailable")?;
                if entries.len() >= request.max_entries as usize {
                    truncated = true;
                    break;
                }
                let meta = std::fs::symlink_metadata(entry.path())
                    .map_err(|_| "Local metadata unavailable")?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| "Local name is not UTF-8")?;
                let path = entry
                    .path()
                    .to_str()
                    .ok_or("Local path is not UTF-8")?
                    .to_owned();
                let mut kind = if meta.file_type().is_symlink() {
                    3
                } else if meta.is_dir() {
                    2
                } else if meta.is_file() {
                    1
                } else {
                    4
                };
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if meta.file_attributes() & 0x400 != 0 {
                        kind = 3;
                    }
                }
                if !valid_path(&path) || !valid_path(&name) {
                    return Err("Local entry exceeds path limit");
                }
                entries.push(SftpDirectoryEntry {
                    name,
                    path,
                    kind,
                    size: meta.is_file().then_some(meta.len()),
                    modified_unix_seconds: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .and_then(|t| u32::try_from(t.as_secs()).ok()),
                    ..Default::default()
                });
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(SftpResponse {
                entries,
                truncated,
                document_json: serde_json::to_vec(
                    path.to_str().ok_or("Local directory is not UTF-8")?,
                )
                .map_err(|_| "Local directory encoding failed")?,
                ..Default::default()
            })
        })
        .await;
        match result {
            Ok(Ok(reply)) => reply,
            _ => SftpResponse::with_status(
                SftpStatus::Failed,
                "Cannot list a bounded regular local directory",
            ),
        }
    }
}

impl SftpIpcService {
    pub(crate) async fn suspend_session_queue(&self,session:SessionId)->Result<(),SftpStatus> {
        let guard=self.queues.lock(session)?.lock_owned().await;
        let mut snapshot=Queues::load_async(session).await?;
        if !snapshot.paused {snapshot.paused=true;snapshot.changed().map_err(|_|SftpStatus::InvalidRequest)?;Queues::save_owned(&snapshot,guard).await?;}
        Ok(())
    }
}
