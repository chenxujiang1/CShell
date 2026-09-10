use crate::LatestSnapshot;
use cshell_domain::SessionId;
use cshell_ipc::{FrameDelta, FullFrame, envelope};
use cshell_terminal::FrameSnapshot;

const DEFAULT_MAX_DELTA_GENERATION_GAP: u64 = 120;

#[derive(Clone, Debug)]
pub enum SubscriptionFrame {
    Full(FullFrame),
    Delta(FrameDelta),
}

impl SubscriptionFrame {
    #[must_use]
    pub fn generation(&self) -> u64 {
        match self {
            Self::Full(frame) => frame.generation,
            Self::Delta(frame) => frame.generation,
        }
    }

    #[must_use]
    pub fn into_payload(self) -> envelope::Payload {
        match self {
            Self::Full(frame) => envelope::Payload::FullFrame(frame),
            Self::Delta(frame) => envelope::Payload::FrameDelta(frame),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SubscriptionStats {
    pub frames_emitted: u64,
    pub merged_generations: u64,
    pub full_recoveries: u64,
}

/// Refresh-rate-polled, latest-only terminal feed for one client.
///
/// No output snapshots queue up. If the consumer is slow, the next delta is built
/// directly between the last delivered snapshot and the newest snapshot. Large
/// generation gaps, resizes, or deltas larger than a full frame recover with FullFrame.
#[derive(Clone, Debug)]
pub struct TerminalFrameSubscription {
    session_id: SessionId,
    snapshots: LatestSnapshot,
    last_delivered: Option<FrameSnapshot>,
    max_delta_generation_gap: u64,
    stats: SubscriptionStats,
}

impl TerminalFrameSubscription {
    #[must_use]
    pub fn new(session_id: SessionId, snapshots: LatestSnapshot) -> Self {
        Self {
            session_id,
            snapshots,
            last_delivered: None,
            max_delta_generation_gap: DEFAULT_MAX_DELTA_GENERATION_GAP,
            stats: SubscriptionStats::default(),
        }
    }

    #[must_use]
    pub fn stats(&self) -> SubscriptionStats {
        self.stats
    }

    #[must_use]
    pub fn poll(&mut self) -> Option<SubscriptionFrame> {
        let current = self.snapshots.latest()?;
        let Some(base) = self.last_delivered.as_ref() else {
            return Some(self.emit_full(current));
        };
        if current.generation <= base.generation {
            return None;
        }

        let gap = current.generation.saturating_sub(base.generation);
        self.stats.merged_generations = self
            .stats
            .merged_generations
            .saturating_add(gap.saturating_sub(1));
        if gap > self.max_delta_generation_gap
            || (current.rows, current.cols) != (base.rows, base.cols)
        {
            return Some(self.emit_full(current));
        }

        let full = FullFrame::from_terminal_snapshot(self.session_id, &current);
        let Ok(delta) = FrameDelta::between_terminal_snapshots(self.session_id, base, &current)
        else {
            return Some(self.emit_full(current));
        };
        if delta.payload.len() >= full.payload.len() {
            return Some(self.emit_full(current));
        }

        self.last_delivered = Some(current);
        self.stats.frames_emitted = self.stats.frames_emitted.saturating_add(1);
        Some(SubscriptionFrame::Delta(delta))
    }

    #[must_use]
    pub fn force_full(&mut self) -> Option<SubscriptionFrame> {
        self.snapshots
            .latest()
            .map(|snapshot| self.emit_full(snapshot))
    }

    fn emit_full(&mut self, snapshot: FrameSnapshot) -> SubscriptionFrame {
        let frame = FullFrame::from_terminal_snapshot(self.session_id, &snapshot);
        self.last_delivered = Some(snapshot);
        self.stats.frames_emitted = self.stats.frames_emitted.saturating_add(1);
        self.stats.full_recoveries = self.stats.full_recoveries.saturating_add(1);
        SubscriptionFrame::Full(frame)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{SubscriptionFrame, TerminalFrameSubscription};
    use crate::LatestSnapshot;
    use cshell_domain::SessionId;
    use cshell_terminal::{Cell, FrameSnapshot, TerminalModes};

    fn snapshot(generation: u64, character: char) -> FrameSnapshot {
        let mut cells = vec![Cell::default(); 12];
        cells[5].character = character;
        FrameSnapshot {
            generation,
            rows: 3,
            cols: 4,
            cursor_row: 1,
            cursor_col: 2,
            terminal_modes: TerminalModes::default(),
            cells,
        }
    }

    #[test]
    fn slow_consumer_gets_one_merged_delta_instead_of_a_snapshot_backlog() {
        let latest = LatestSnapshot::default();
        latest.publish(snapshot(1, 'A'));
        let mut subscription = TerminalFrameSubscription::new(SessionId::new(), latest.clone());
        let SubscriptionFrame::Full(full) = subscription.poll().unwrap() else {
            panic!("first frame must be full");
        };
        let base = full.decode_terminal_snapshot().unwrap();

        latest.publish(snapshot(2, 'B'));
        latest.publish(snapshot(3, 'C'));
        latest.publish(snapshot(7, 'Z'));
        let SubscriptionFrame::Delta(delta) = subscription.poll().unwrap() else {
            panic!("small gaps should merge into a delta");
        };
        assert_eq!((delta.base_generation, delta.generation), (1, 7));
        assert_eq!(delta.apply_terminal_delta(&base).unwrap(), snapshot(7, 'Z'));
        assert_eq!(subscription.stats().merged_generations, 5);
        assert!(subscription.poll().is_none());
    }

    #[test]
    fn very_slow_consumer_recovers_with_a_full_frame() {
        let latest = LatestSnapshot::default();
        latest.publish(snapshot(1, 'A'));
        let mut subscription = TerminalFrameSubscription::new(SessionId::new(), latest.clone());
        assert!(matches!(
            subscription.poll(),
            Some(SubscriptionFrame::Full(_))
        ));

        latest.publish(snapshot(500, 'Z'));
        assert!(matches!(
            subscription.poll(),
            Some(SubscriptionFrame::Full(_))
        ));
        assert_eq!(subscription.stats().full_recoveries, 2);
    }
}
