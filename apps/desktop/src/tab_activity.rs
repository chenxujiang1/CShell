//! One unread bit per open tab; existing snapshots supply activity without storing output.
use cshell_domain::{SessionId, TabId};
use cshell_terminal::FrameSnapshot;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub(crate) struct TabActivity {
    seen: BTreeMap<TabId, (SessionId, u64, u16, u16)>,
    unread: BTreeSet<TabId>,
}
impl TabActivity {
    pub(crate) fn observe(
        &mut self,
        tab: TabId,
        session: SessionId,
        frame: &FrameSnapshot,
        viewed: bool,
    ) {
        let next = (session, frame.generation, frame.rows, frame.cols);
        if let Some(previous) = self.seen.insert(tab, next) {
            if previous.0 != session || frame.generation < previous.1 {
                self.unread.remove(&tab);
            } else if !viewed
                && previous.1 != frame.generation
                && previous.2 == frame.rows
                && previous.3 == frame.cols
            {
                self.unread.insert(tab);
            }
        }
        if viewed {
            self.unread.remove(&tab);
        }
    }
    pub(crate) fn refresh(
        &mut self,
        opened: &BTreeSet<TabId>,
        viewed: &BTreeSet<TabId>,
    ) -> BTreeSet<TabId> {
        self.seen.retain(|tab, _| opened.contains(tab));
        self.unread
            .retain(|tab| opened.contains(tab) && !viewed.contains(tab));
        self.unread.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cshell_terminal::{ProbeTerminalEngine, TerminalEngine};
    #[test]
    fn background_output_marks_once_viewing_clears_and_resize_does_not_mark() {
        let tab = TabId::new();
        let session = SessionId::new();
        let mut engine = ProbeTerminalEngine::new(cshell_domain::TerminalSize::cells(3, 20));
        let mut activity = TabActivity::default();
        activity.observe(tab, session, &engine.snapshot(), false);
        assert!(activity.unread.is_empty());
        engine.feed(b"output");
        activity.observe(tab, session, &engine.snapshot(), false);
        assert!(activity.unread.contains(&tab));
        activity.observe(tab, session, &engine.snapshot(), true);
        assert!(activity.unread.is_empty());
        engine.resize(cshell_domain::TerminalSize {
            rows: 4,
            cols: 20,
            pixel_width: 0,
            pixel_height: 0,
            generation: 1,
        });
        activity.observe(tab, session, &engine.snapshot(), false);
        assert!(activity.unread.is_empty());
        engine.feed(b"more");
        activity.observe(tab, session, &engine.snapshot(), false);
        assert!(activity.unread.contains(&tab));
        activity.observe(tab, SessionId::new(), &engine.snapshot(), false);
        assert!(activity.unread.is_empty());
        activity.refresh(&BTreeSet::new(), &BTreeSet::new());
        assert!(activity.seen.is_empty());
    }
}
