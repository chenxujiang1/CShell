use crate::{
    LocalSessionError, LocalSessionExit, LocalTerminalHandle, LocalTerminalSession,
    TerminalFrameSubscription,
};
use cshell_domain::{InputAction, SessionId, TerminalSize};
use cshell_ipc::{FullFrame, LogPageCodecError, LogPageRequest, SnapshotRequest};
use cshell_local::LocalProfile;
use cshell_output_store::{JournalError, JournalStyledLogPage};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SessionRegistryError {
    #[error("cannot prepare terminal journal directory: {0}")]
    JournalDirectory(std::io::Error),
    #[error(transparent)]
    Session(#[from] LocalSessionError),
    #[error("terminal session {0} does not exist")]
    UnknownSession(SessionId),
    #[error("terminal session {0} has no snapshot")]
    SnapshotUnavailable(SessionId),
    #[error("IPC session ID must contain exactly 16 bytes, got {0}")]
    InvalidSessionIdLength(usize),
    #[error("terminal size {rows}x{cols} is outside the supported range")]
    InvalidTerminalSize { rows: u32, cols: u32 },
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error(transparent)]
    LogPageProtocol(#[from] LogPageCodecError),
}

#[derive(Debug)]
struct ManagedLocalSession {
    title: String,
    session: LocalTerminalSession,
    exit: Option<LocalSessionExit>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSessionInfo {
    pub session_id: SessionId,
    pub title: String,
    pub running: bool,
    pub generation: u64,
}

/// A GUI/CLI attachment that never owns the daemon session itself.
///
/// Dropping every attachment leaves the registered PTY running. A later client can
/// attach again and request the latest complete terminal frame.
#[derive(Clone, Debug)]
pub struct LocalSessionAttachment {
    session_id: SessionId,
    handle: LocalTerminalHandle,
}

impl LocalSessionAttachment {
    #[must_use]
    pub const fn session_id(&self) -> SessionId {
        self.session_id
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }

    pub fn send_input(&self, action: &InputAction) -> Result<(), LocalSessionError> {
        self.handle.send_input(action)
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), LocalSessionError> {
        self.handle.resize(size)
    }

    pub fn full_frame(&self) -> Result<FullFrame, SessionRegistryError> {
        let snapshot = self
            .handle
            .snapshots()
            .latest()
            .ok_or(SessionRegistryError::SnapshotUnavailable(self.session_id))?;
        Ok(FullFrame::from_terminal_snapshot(
            self.session_id,
            &snapshot,
        ))
    }
}

/// Daemon session directory with short global lock scope and per-session locking.
#[derive(Debug)]
pub struct LocalSessionRegistry {
    journal_root: PathBuf,
    ingress_capacity: usize,
    sessions: RwLock<BTreeMap<SessionId, Arc<Mutex<ManagedLocalSession>>>>,
}

impl LocalSessionRegistry {
    pub fn new(
        journal_root: impl AsRef<Path>,
        ingress_capacity: usize,
    ) -> Result<Self, SessionRegistryError> {
        let journal_root = journal_root.as_ref().to_path_buf();
        std::fs::create_dir_all(&journal_root).map_err(SessionRegistryError::JournalDirectory)?;
        Ok(Self {
            journal_root,
            ingress_capacity: ingress_capacity.max(1),
            sessions: RwLock::new(BTreeMap::new()),
        })
    }

    pub fn spawn_local(
        &self,
        profile: &LocalProfile,
        size: TerminalSize,
    ) -> Result<LocalSessionAttachment, SessionRegistryError> {
        let session_id = SessionId::new();
        let journal_path = self.journal_path(session_id);
        let session =
            LocalTerminalSession::spawn(profile, size, journal_path, self.ingress_capacity)?;
        let attachment = LocalSessionAttachment {
            session_id,
            handle: session.handle(),
        };
        let managed = Arc::new(Mutex::new(ManagedLocalSession {
            title: profile.name.clone(),
            session,
            exit: None,
        }));
        self.sessions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id, managed);
        Ok(attachment)
    }

    pub fn spawn_platform_default(
        &self,
        rows: u32,
        cols: u32,
    ) -> Result<LocalSessionInfo, SessionRegistryError> {
        let rows = u16::try_from(rows)
            .ok()
            .filter(|rows| *rows > 0)
            .ok_or(SessionRegistryError::InvalidTerminalSize { rows, cols })?;
        let cols = u16::try_from(cols).ok().filter(|cols| *cols > 0).ok_or(
            SessionRegistryError::InvalidTerminalSize {
                rows: u32::from(rows),
                cols,
            },
        )?;
        if usize::from(rows).saturating_mul(usize::from(cols)) > 1_000_000 {
            return Err(SessionRegistryError::InvalidTerminalSize {
                rows: u32::from(rows),
                cols: u32::from(cols),
            });
        }
        let attachment = self.spawn_local(
            &LocalProfile::platform_default(),
            TerminalSize::cells(rows, cols),
        )?;
        self.session_info(attachment.session_id())
    }

    pub fn attach(
        &self,
        session_id: SessionId,
    ) -> Result<LocalSessionAttachment, SessionRegistryError> {
        let managed = self.lookup(session_id)?;
        let handle = managed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session
            .handle();
        Ok(LocalSessionAttachment { session_id, handle })
    }

    /// Resolves an IPC recovery request to the latest complete frame.
    pub fn fulfill_snapshot_request(
        &self,
        request: &SnapshotRequest,
    ) -> Result<FullFrame, SessionRegistryError> {
        self.attach(Self::request_session_id(request)?)?
            .full_frame()
    }

    pub fn subscribe_request(
        &self,
        request: &SnapshotRequest,
    ) -> Result<TerminalFrameSubscription, SessionRegistryError> {
        self.subscribe(Self::request_session_id(request)?)
    }

    pub fn subscribe(
        &self,
        session_id: SessionId,
    ) -> Result<TerminalFrameSubscription, SessionRegistryError> {
        let attachment = self.attach(session_id)?;
        Ok(TerminalFrameSubscription::new(
            session_id,
            attachment.handle.snapshots(),
        ))
    }

    pub fn fulfill_log_page_request(
        &self,
        request: &LogPageRequest,
    ) -> Result<JournalStyledLogPage, SessionRegistryError> {
        request.validate()?;
        let session_id = Self::parse_session_id(&request.session_id)?;
        let index = self
            .lookup(session_id)?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session
            .line_index();
        index
            .read_styled_page(
                request.anchor_line_id,
                request.rows_before as usize,
                request.rows_after as usize,
            )
            .map_err(Into::into)
    }

    /// Non-blockingly observes and finalizes a child exit while retaining its last frame.
    pub fn poll_exit(
        &self,
        session_id: SessionId,
    ) -> Result<Option<LocalSessionExit>, SessionRegistryError> {
        let managed = self.lookup(session_id)?;
        let mut managed = managed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if managed.exit.is_none() {
            managed.exit = managed.session.try_wait()?;
        }
        Ok(managed.exit.clone())
    }

    /// Observes every registered child without holding the registry lock while
    /// individual sessions finalize their worker threads.
    pub fn poll_exits(&self) -> Result<Vec<(SessionId, LocalSessionExit)>, SessionRegistryError> {
        let entries: Vec<_> = self
            .sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(session_id, managed)| (*session_id, Arc::clone(managed)))
            .collect();
        let mut exits = Vec::new();
        for (session_id, managed) in entries {
            let mut managed = managed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if managed.exit.is_none()
                && let Some(exit) = managed.session.try_wait()?
            {
                managed.exit = Some(exit.clone());
                exits.push((session_id, exit));
            }
        }
        Ok(exits)
    }

    /// Removes a session. A running child is terminated before its registry entry is lost.
    pub fn close(
        &self,
        session_id: SessionId,
    ) -> Result<Option<LocalSessionExit>, SessionRegistryError> {
        let managed = self
            .sessions
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&session_id)
            .ok_or(SessionRegistryError::UnknownSession(session_id))?;
        let mut managed = managed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if managed.exit.is_none() {
            managed.exit = managed.session.try_wait()?;
        }
        if managed.exit.is_none() {
            managed.session.kill()?;
        }
        Ok(managed.exit.clone())
    }

    pub fn list(&self) -> Result<Vec<LocalSessionInfo>, SessionRegistryError> {
        let entries: Vec<_> = self
            .sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(session_id, managed)| (*session_id, Arc::clone(managed)))
            .collect();
        entries
            .into_iter()
            .map(|(session_id, managed)| {
                let mut managed = managed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if managed.exit.is_none() {
                    managed.exit = managed.session.try_wait()?;
                }
                Ok(Self::managed_info(session_id, &managed))
            })
            .collect()
    }

    pub fn session_info(
        &self,
        session_id: SessionId,
    ) -> Result<LocalSessionInfo, SessionRegistryError> {
        let managed = self.lookup(session_id)?;
        let mut managed = managed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if managed.exit.is_none() {
            managed.exit = managed.session.try_wait()?;
        }
        Ok(Self::managed_info(session_id, &managed))
    }

    #[must_use]
    pub fn contains(&self, session_id: SessionId) -> bool {
        self.sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&session_id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn session_ids(&self) -> Vec<SessionId> {
        self.sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect()
    }

    #[must_use]
    pub fn journal_path(&self, session_id: SessionId) -> PathBuf {
        self.journal_root.join(format!("{session_id}.csjr"))
    }

    fn lookup(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<Mutex<ManagedLocalSession>>, SessionRegistryError> {
        self.sessions
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned()
            .ok_or(SessionRegistryError::UnknownSession(session_id))
    }

    fn request_session_id(request: &SnapshotRequest) -> Result<SessionId, SessionRegistryError> {
        let bytes: [u8; 16] =
            request.session_id.as_slice().try_into().map_err(|_| {
                SessionRegistryError::InvalidSessionIdLength(request.session_id.len())
            })?;
        Ok(SessionId::from_bytes(bytes))
    }

    pub fn parse_session_id(bytes: &[u8]) -> Result<SessionId, SessionRegistryError> {
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| SessionRegistryError::InvalidSessionIdLength(bytes.len()))?;
        Ok(SessionId::from_bytes(bytes))
    }

    fn managed_info(session_id: SessionId, managed: &ManagedLocalSession) -> LocalSessionInfo {
        LocalSessionInfo {
            session_id,
            title: managed.title.clone(),
            running: managed.exit.is_none(),
            generation: managed
                .session
                .snapshots()
                .latest()
                .map_or(0, |snapshot| snapshot.generation),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{LocalSessionRegistry, SessionRegistryError};
    use cshell_domain::TerminalSize;
    use cshell_ipc::SnapshotRequest;
    use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn reconnect_probe_profile() -> LocalProfile {
        #[cfg(windows)]
        return LocalProfile {
            name: "registry reconnect probe".to_owned(),
            program: PathBuf::from("cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "echo CSHELL_RECONNECT_READY & ping -n 2 127.0.0.1 >nul & echo CSHELL_RECONNECT_DONE"
                    .to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        LocalProfile {
            name: "registry reconnect probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-lc".to_owned(),
                "printf CSHELL_RECONNECT_READY; sleep 1; printf CSHELL_RECONNECT_DONE".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        }
    }

    #[test]
    fn a_new_attachment_recovers_full_frame_after_the_first_client_disconnects() {
        let directory = tempfile::tempdir().unwrap();
        let registry = LocalSessionRegistry::new(directory.path(), 32).unwrap();
        let first = registry
            .spawn_local(&reconnect_probe_profile(), TerminalSize::cells(24, 80))
            .unwrap();
        let session_id = first.session_id();
        let snapshots = first.handle.snapshots();
        let ready_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let ready = snapshots.latest().is_some_and(|snapshot| {
                snapshot
                    .cells
                    .iter()
                    .map(|cell| cell.character)
                    .collect::<String>()
                    .contains("CSHELL_RECONNECT_READY")
            });
            if ready {
                break;
            }
            assert!(
                Instant::now() < ready_deadline,
                "registered PTY did not produce its ready marker"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!first.is_closed());
        drop(first);

        assert!(registry.contains(session_id));
        assert_eq!(registry.session_ids(), vec![session_id]);
        let second = registry.attach(session_id).unwrap();
        let frame = registry
            .fulfill_snapshot_request(&SnapshotRequest {
                session_id: session_id.as_uuid().as_bytes().to_vec(),
                current_generation: None,
            })
            .unwrap();
        let snapshot = frame.decode_terminal_snapshot().unwrap();
        let visible: String = snapshot.cells.iter().map(|cell| cell.character).collect();
        assert!(visible.contains("CSHELL_RECONNECT_READY"));
        drop(second);

        let deadline = Instant::now() + Duration::from_secs(5);
        let exit = loop {
            if let Some(exit) = registry.poll_exit(session_id).unwrap() {
                break exit;
            }
            assert!(Instant::now() < deadline, "registered PTY did not exit");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(exit.success, "PTY exit: {exit:?}");

        let after_exit = registry.attach(session_id).unwrap();
        assert!(after_exit.is_closed());
        let final_snapshot = after_exit
            .full_frame()
            .unwrap()
            .decode_terminal_snapshot()
            .unwrap();
        let visible: String = final_snapshot
            .cells
            .iter()
            .map(|cell| cell.character)
            .collect();
        assert!(visible.contains("CSHELL_RECONNECT_DONE"));

        assert_eq!(registry.close(session_id).unwrap(), Some(exit));
        assert!(registry.is_empty());
        assert!(matches!(
            registry.attach(session_id),
            Err(SessionRegistryError::UnknownSession(id)) if id == session_id
        ));
    }

    #[test]
    fn malformed_snapshot_request_session_id_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let registry = LocalSessionRegistry::new(directory.path(), 8).unwrap();
        let request = SnapshotRequest {
            session_id: vec![7; 15],
            current_generation: None,
        };

        assert!(matches!(
            registry.fulfill_snapshot_request(&request),
            Err(SessionRegistryError::InvalidSessionIdLength(15))
        ));
    }
}
