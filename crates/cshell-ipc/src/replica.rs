use crate::{DeltaCodecError, FrameDelta, FullFrame, SnapshotCodecError, SnapshotRequest};
use cshell_terminal::FrameSnapshot;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub enum ApplyResult {
    Applied,
    IgnoredStale,
    NeedFullSnapshot(SnapshotRequest),
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ReplicaError {
    #[error("frame belongs to a different session")]
    SessionMismatch,
    #[error("delta generation must be greater than its base generation")]
    InvalidGeneration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaState {
    session_id: Vec<u8>,
    generation: Option<u64>,
    payload: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum TerminalReplicaError {
    #[error("frame belongs to a different terminal session")]
    SessionMismatch,
    #[error(transparent)]
    FullFrame(#[from] SnapshotCodecError),
    #[error(transparent)]
    Delta(#[from] DeltaCodecError),
}

#[derive(Clone, Debug)]
pub struct TerminalReplicaState {
    session_id: Vec<u8>,
    snapshot: Option<FrameSnapshot>,
}

impl TerminalReplicaState {
    #[must_use]
    pub fn new(session_id: Vec<u8>) -> Self {
        Self {
            session_id,
            snapshot: None,
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<&FrameSnapshot> {
        self.snapshot.as_ref()
    }

    pub fn apply_full(&mut self, frame: FullFrame) -> Result<ApplyResult, TerminalReplicaError> {
        self.ensure_session(&frame.session_id)?;
        if self
            .snapshot
            .as_ref()
            .is_some_and(|current| frame.generation < current.generation)
        {
            return Ok(ApplyResult::IgnoredStale);
        }
        self.snapshot = Some(frame.decode_terminal_snapshot()?);
        Ok(ApplyResult::Applied)
    }

    pub fn apply_delta(&mut self, delta: FrameDelta) -> Result<ApplyResult, TerminalReplicaError> {
        self.ensure_session(&delta.session_id)?;
        let Some(snapshot) = self.snapshot.as_ref() else {
            return Ok(self.request_full_snapshot());
        };
        if snapshot.generation != delta.base_generation {
            return Ok(self.request_full_snapshot());
        }
        self.snapshot = Some(delta.apply_terminal_delta(snapshot)?);
        Ok(ApplyResult::Applied)
    }

    fn request_full_snapshot(&self) -> ApplyResult {
        ApplyResult::NeedFullSnapshot(SnapshotRequest {
            session_id: self.session_id.clone(),
            current_generation: self.snapshot.as_ref().map(|snapshot| snapshot.generation),
        })
    }

    fn ensure_session(&self, session_id: &[u8]) -> Result<(), TerminalReplicaError> {
        if self.session_id == session_id {
            Ok(())
        } else {
            Err(TerminalReplicaError::SessionMismatch)
        }
    }
}

impl ReplicaState {
    #[must_use]
    pub fn new(session_id: Vec<u8>) -> Self {
        Self {
            session_id,
            generation: None,
            payload: Vec::new(),
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        self.generation
    }

    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn apply_full(&mut self, frame: FullFrame) -> Result<ApplyResult, ReplicaError> {
        self.ensure_session(&frame.session_id)?;
        if self
            .generation
            .is_some_and(|current| frame.generation < current)
        {
            return Ok(ApplyResult::IgnoredStale);
        }
        self.generation = Some(frame.generation);
        self.payload = frame.payload;
        Ok(ApplyResult::Applied)
    }

    pub fn apply_delta(&mut self, delta: FrameDelta) -> Result<ApplyResult, ReplicaError> {
        self.ensure_session(&delta.session_id)?;
        if delta.generation <= delta.base_generation {
            return Err(ReplicaError::InvalidGeneration);
        }
        if self.generation != Some(delta.base_generation) {
            return Ok(ApplyResult::NeedFullSnapshot(SnapshotRequest {
                session_id: self.session_id.clone(),
                current_generation: self.generation,
            }));
        }
        self.generation = Some(delta.generation);
        self.payload = delta.payload;
        Ok(ApplyResult::Applied)
    }

    fn ensure_session(&self, session_id: &[u8]) -> Result<(), ReplicaError> {
        if self.session_id == session_id {
            Ok(())
        } else {
            Err(ReplicaError::SessionMismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplyResult, ReplicaState, TerminalReplicaState};
    use crate::{FrameDelta, FullFrame, SnapshotRequest};
    use cshell_domain::SessionId;
    use cshell_terminal::{Cell, FrameSnapshot, TerminalModes};

    #[test]
    fn mismatched_delta_requests_a_full_snapshot() {
        let session_id = vec![3; 16];
        let mut replica = ReplicaState::new(session_id.clone());
        replica
            .apply_full(FullFrame {
                session_id: session_id.clone(),
                generation: 4,
                payload: b"full-4".to_vec(),
            })
            .unwrap_or_else(|error| panic!("{error}"));
        let result = replica
            .apply_delta(FrameDelta {
                session_id,
                base_generation: 5,
                generation: 6,
                payload: b"delta-6".to_vec(),
            })
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(result, ApplyResult::NeedFullSnapshot(_)));
        assert_eq!(replica.generation(), Some(4));
        assert_eq!(replica.payload(), b"full-4");
    }

    #[test]
    fn matching_delta_advances_generation() {
        let session_id = vec![3; 16];
        let mut replica = ReplicaState::new(session_id.clone());
        replica
            .apply_full(FullFrame {
                session_id: session_id.clone(),
                generation: 4,
                payload: Vec::new(),
            })
            .unwrap_or_else(|error| panic!("{error}"));
        let result = replica
            .apply_delta(FrameDelta {
                session_id,
                base_generation: 4,
                generation: 5,
                payload: b"delta-5".to_vec(),
            })
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(result, ApplyResult::Applied);
        assert_eq!(replica.generation(), Some(5));
    }

    #[test]
    fn typed_terminal_replica_applies_delta_and_recovers_from_a_gap() {
        let session_id = SessionId::new();
        let mut base = FrameSnapshot {
            generation: 3,
            rows: 2,
            cols: 3,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: Default::default(),
            terminal_modes: TerminalModes::default(),
            cells: vec![Cell::default(); 6],
        };
        let mut current = base.clone();
        current.generation = 4;
        current.cells[4].character = 'X';
        let full = FullFrame::from_terminal_snapshot(session_id, &base);
        let delta = FrameDelta::between_terminal_snapshots(session_id, &base, &current)
            .unwrap_or_else(|error| panic!("{error}"));

        let mut replica = TerminalReplicaState::new(session_id.as_uuid().as_bytes().to_vec());
        assert_eq!(
            replica
                .apply_full(full)
                .unwrap_or_else(|error| panic!("{error}")),
            ApplyResult::Applied
        );
        assert_eq!(
            replica
                .apply_delta(delta)
                .unwrap_or_else(|error| panic!("{error}")),
            ApplyResult::Applied
        );
        assert_eq!(replica.snapshot(), Some(&current));

        base.generation = 8;
        let mut future = base.clone();
        future.generation = 9;
        let gap = FrameDelta::between_terminal_snapshots(session_id, &base, &future)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            replica
                .apply_delta(gap)
                .unwrap_or_else(|error| panic!("{error}")),
            ApplyResult::NeedFullSnapshot(SnapshotRequest {
                current_generation: Some(4),
                ..
            })
        ));
    }
}
