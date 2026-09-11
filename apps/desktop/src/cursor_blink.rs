use cshell_terminal::{CursorAppearance, CursorShape, FrameSnapshot};
use std::time::{Duration, Instant};

const DEFAULT_BLINK_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CursorObservation {
    generation: u64,
    appearance: CursorAppearance,
}

#[derive(Debug)]
pub struct CursorBlinkState {
    observation: Option<CursorObservation>,
    active: bool,
    visible: bool,
    next_toggle: Option<Instant>,
    interval: Duration,
}

impl Default for CursorBlinkState {
    fn default() -> Self {
        Self {
            observation: None,
            active: false,
            visible: true,
            next_toggle: None,
            interval: DEFAULT_BLINK_INTERVAL,
        }
    }
}

impl CursorBlinkState {
    /// Synchronizes the scheduler with the latest immutable terminal frame.
    /// A new frame or focus change restarts the visible phase so output never
    /// arrives behind an invisible cursor.
    pub fn synchronize(
        &mut self,
        snapshot: Option<&FrameSnapshot>,
        window_active: bool,
        now: Instant,
    ) -> bool {
        let observation = snapshot.map(|snapshot| CursorObservation {
            generation: snapshot.generation,
            appearance: snapshot.cursor_appearance,
        });
        let active = window_active && observation.is_some_and(CursorObservation::requests_blink);

        if self.observation != observation {
            self.observation = observation;
            self.active = active;
            self.visible = true;
            self.next_toggle = active.then_some(now + self.interval);
            return true;
        }
        if self.active != active {
            return self.set_window_active(window_active, now);
        }

        if self.active && self.next_toggle.is_some_and(|deadline| now >= deadline) {
            self.visible = !self.visible;
            self.next_toggle = Some(now + self.interval);
            return true;
        }
        false
    }

    pub fn set_window_active(&mut self, window_active: bool, now: Instant) -> bool {
        let active = window_active
            && self
                .observation
                .is_some_and(CursorObservation::requests_blink);
        if self.active == active {
            return false;
        }
        self.active = active;
        self.visible = true;
        self.next_toggle = active.then_some(now + self.interval);
        true
    }

    /// Keyboard/IME activity restarts the visible half-cycle without changing
    /// terminal state or allocating a replacement frame.
    pub fn note_activity(&mut self, now: Instant) -> bool {
        if !self.active {
            return false;
        }
        let changed = !self.visible;
        self.visible = true;
        self.next_toggle = Some(now + self.interval);
        changed
    }

    #[must_use]
    pub const fn visible(&self) -> bool {
        self.visible
    }

    #[must_use]
    pub const fn next_toggle(&self) -> Option<Instant> {
        self.next_toggle
    }
}

impl CursorObservation {
    fn requests_blink(self) -> bool {
        self.appearance.blinking && self.appearance.shape != CursorShape::Hidden
    }
}

#[cfg(test)]
mod tests {
    use super::CursorBlinkState;
    use cshell_terminal::{CursorAppearance, CursorShape, FrameSnapshot, TerminalModes};
    use std::time::{Duration, Instant};

    fn snapshot(generation: u64, shape: CursorShape, blinking: bool) -> FrameSnapshot {
        FrameSnapshot {
            generation,
            rows: 1,
            cols: 1,
            cursor_row: 0,
            cursor_col: 0,
            cursor_appearance: CursorAppearance {
                shape,
                blinking,
                color: None,
            },
            terminal_modes: TerminalModes::default(),
            cells: vec![Default::default()],
        }
    }

    #[test]
    fn blinking_cursor_toggles_only_at_deadlines_and_activity_resets_visibility() {
        let started = Instant::now();
        let frame = snapshot(1, CursorShape::Beam, true);
        let mut blink = CursorBlinkState::default();

        assert!(blink.synchronize(Some(&frame), true, started));
        assert!(blink.visible());
        assert_eq!(
            blink.next_toggle(),
            Some(started + Duration::from_millis(500))
        );
        assert!(!blink.synchronize(Some(&frame), true, started + Duration::from_millis(499)));
        assert!(blink.visible());
        assert!(blink.synchronize(Some(&frame), true, started + Duration::from_millis(500)));
        assert!(!blink.visible());

        assert!(blink.note_activity(started + Duration::from_millis(700)));
        assert!(blink.visible());
        assert_eq!(
            blink.next_toggle(),
            Some(started + Duration::from_millis(1_200))
        );
    }

    #[test]
    fn new_frames_focus_loss_and_hidden_shapes_leave_the_cursor_visible_and_idle() {
        let started = Instant::now();
        let mut blink = CursorBlinkState::default();
        let first = snapshot(1, CursorShape::Block, true);
        blink.synchronize(Some(&first), true, started);
        blink.synchronize(Some(&first), true, started + Duration::from_millis(500));
        assert!(!blink.visible());

        let second = snapshot(2, CursorShape::Block, true);
        assert!(blink.synchronize(Some(&second), true, started + Duration::from_millis(600)));
        assert!(blink.visible());
        assert!(blink.synchronize(Some(&second), false, started + Duration::from_millis(700)));
        assert!(blink.visible());
        assert_eq!(blink.next_toggle(), None);

        let hidden = snapshot(3, CursorShape::Hidden, true);
        assert!(blink.synchronize(Some(&hidden), true, started + Duration::from_millis(800)));
        assert!(blink.visible());
        assert_eq!(blink.next_toggle(), None);
    }
}
