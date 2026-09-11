use crate::{
    ApplyResult, Envelope, SnapshotRequest, TerminalReplicaError, TerminalReplicaState, envelope,
};
use cshell_domain::SessionId;
use cshell_terminal::FrameSnapshot;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub enum ClientFrameUpdate {
    Applied { generation: u64 },
    IgnoredStale,
    RequestFull(SnapshotRequest),
}

#[derive(Debug, Error)]
pub enum SubscriptionClientError {
    #[error("terminal subscription received an unsupported IPC payload")]
    UnsupportedPayload,
    #[error(transparent)]
    TerminalReplica(#[from] TerminalReplicaError),
}

/// GUI-side protocol state for one reconnectable terminal subscription.
#[derive(Clone, Debug)]
pub struct TerminalSubscriptionReplica {
    session_id: SessionId,
    replica: TerminalReplicaState,
}

impl TerminalSubscriptionReplica {
    #[must_use]
    pub fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            replica: TerminalReplicaState::new(session_id.as_uuid().as_bytes().to_vec()),
        }
    }

    #[must_use]
    pub fn snapshot_request(&self) -> SnapshotRequest {
        SnapshotRequest {
            session_id: self.session_id.as_uuid().as_bytes().to_vec(),
            current_generation: self.replica.snapshot().map(|snapshot| snapshot.generation),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<&FrameSnapshot> {
        self.replica.snapshot()
    }

    pub fn apply_envelope(
        &mut self,
        envelope: Envelope,
    ) -> Result<ClientFrameUpdate, SubscriptionClientError> {
        let result = match envelope.payload {
            Some(envelope::Payload::FullFrame(frame)) => self.replica.apply_full(frame)?,
            Some(envelope::Payload::FrameDelta(delta)) => self.replica.apply_delta(delta)?,
            _ => return Err(SubscriptionClientError::UnsupportedPayload),
        };
        Ok(match result {
            ApplyResult::Applied => ClientFrameUpdate::Applied {
                generation: self
                    .replica
                    .snapshot()
                    .map_or(0, |snapshot| snapshot.generation),
            },
            ApplyResult::IgnoredStale => ClientFrameUpdate::IgnoredStale,
            ApplyResult::NeedFullSnapshot(request) => ClientFrameUpdate::RequestFull(request),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ClientFrameUpdate, TerminalSubscriptionReplica};
    use crate::{Envelope, FrameDelta, FullFrame, SnapshotRequest, envelope};
    use cshell_domain::SessionId;
    use cshell_terminal::{Cell, FrameSnapshot, TerminalModes};

    fn snapshot(generation: u64, character: char) -> FrameSnapshot {
        let mut cells = vec![Cell::default(); 4];
        cells[0].character = character;
        FrameSnapshot {
            generation,
            rows: 2,
            cols: 2,
            cursor_row: 0,
            cursor_col: 1,
            cursor_appearance: Default::default(),
            terminal_modes: TerminalModes::default(),
            cells,
        }
    }

    #[test]
    fn client_applies_stream_and_requests_full_frame_on_generation_gap() {
        let session_id = SessionId::new();
        let base = snapshot(1, 'A');
        let current = snapshot(2, 'B');
        let mut client = TerminalSubscriptionReplica::new(session_id);
        assert_eq!(
            client
                .apply_envelope(Envelope {
                    request_id: 1,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::FullFrame(
                        FullFrame::from_terminal_snapshot(session_id, &base),
                    )),
                })
                .unwrap(),
            ClientFrameUpdate::Applied { generation: 1 }
        );
        let delta = FrameDelta::between_terminal_snapshots(session_id, &base, &current).unwrap();
        assert_eq!(
            client
                .apply_envelope(Envelope {
                    request_id: 0,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::FrameDelta(delta)),
                })
                .unwrap(),
            ClientFrameUpdate::Applied { generation: 2 }
        );

        let future_base = snapshot(8, 'X');
        let future = snapshot(9, 'Y');
        let gap =
            FrameDelta::between_terminal_snapshots(session_id, &future_base, &future).unwrap();
        assert!(matches!(
            client
                .apply_envelope(Envelope {
                    request_id: 0,
                    deadline_unix_ms: 0,
                    payload: Some(envelope::Payload::FrameDelta(gap)),
                })
                .unwrap(),
            ClientFrameUpdate::RequestFull(SnapshotRequest {
                current_generation: Some(2),
                ..
            })
        ));
    }
}
