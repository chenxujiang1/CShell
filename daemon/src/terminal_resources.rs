//! Process-wide history admission and byte-counted output backpressure.
use crate::AlacrittyTerminalEngine;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
pub(crate) const GLOBAL_HISTORY_BYTES: usize = 512 * 1024 * 1024;
pub(crate) const OUTPUT_QUEUE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
#[derive(Debug)]
struct HistoryEntry {
    engine: Weak<Mutex<AlacrittyTerminalEngine>>,
    subscribers: Weak<std::sync::atomic::AtomicUsize>,
    active: bool,
}
#[derive(Debug)]
pub(crate) struct HistoryPool {
    limit: usize,
    engines: Mutex<Vec<HistoryEntry>>,
}
impl HistoryPool {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            engines: Mutex::new(Vec::new()),
        }
    }
    #[cfg(test)]
    fn register(&self, engine: &Arc<Mutex<AlacrittyTerminalEngine>>) {
        self.register_with_activity(engine, &Arc::new(std::sync::atomic::AtomicUsize::new(0)));
    }
    fn register_with_activity(
        &self,
        engine: &Arc<Mutex<AlacrittyTerminalEngine>>,
        subscribers: &Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let mut entries = self
            .engines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries.retain(|entry| entry.engine.strong_count() > 0);
        entries.push(HistoryEntry {
            engine: Arc::downgrade(engine),
            subscribers: Arc::downgrade(subscribers),
            active: false,
        });
        Self::refresh_activity(&mut entries);
        self.apply_quotas(&entries);
    }
    fn refresh_activity(entries: &mut [HistoryEntry]) -> bool {
        let mut changed = false;
        for entry in entries {
            let active = entry
                .subscribers
                .upgrade()
                .is_some_and(|count| count.load(std::sync::atomic::Ordering::Relaxed) > 0);
            changed |= active != entry.active;
            entry.active = active;
        }
        changed
    }
    fn apply_quotas(&self, entries: &[HistoryEntry]) {
        let weight = entries
            .iter()
            .map(|entry| if entry.active { 2 } else { 1 })
            .sum::<usize>()
            .max(1);
        // Always reclaim reductions before permitting growth. Visible subscribers
        // get twice the share of background sessions, each capped at 64 MiB.
        for reductions in [true, false] {
            for entry in entries {
                if let Some(engine) = entry.engine.upgrade() {
                    let quota = (self.limit / weight * if entry.active { 2 } else { 1 })
                        .min(cshell_terminal::DEFAULT_HISTORY_BYTES);
                    let mut engine = engine
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if (quota < engine.history_budget_bytes()) == reductions {
                        engine.set_history_budget(quota);
                    }
                }
            }
        }
    }
    fn refresh(&self) {
        let mut entries = self
            .engines
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = entries.len();
        entries.retain(|entry| entry.engine.strong_count() > 0);
        let changed = Self::refresh_activity(&mut entries);
        if entries.len() != previous || changed {
            self.apply_quotas(&entries);
        }
    }
}
static POOL: OnceLock<HistoryPool> = OnceLock::new();
pub(crate) fn register_history(
    engine: &Arc<Mutex<AlacrittyTerminalEngine>>,
    subscribers: &Arc<std::sync::atomic::AtomicUsize>,
) {
    POOL.get_or_init(|| HistoryPool::new(GLOBAL_HISTORY_BYTES))
        .register_with_activity(engine, subscribers);
}
pub(crate) fn refresh_history() {
    if let Some(pool) = POOL.get() {
        pool.refresh();
    }
}
#[derive(Debug, Default)]
struct QueueState {
    bytes: usize,
    peak: usize,
    closed: bool,
}
#[derive(Debug, Default)]
pub(crate) struct ByteQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}
impl ByteQueue {
    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize, blocking: bool) -> Option<BytePermit> {
        if bytes > OUTPUT_QUEUE_BYTES {
            return None;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.closed && state.bytes + bytes > OUTPUT_QUEUE_BYTES {
            if !blocking {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.closed {
            return None;
        }
        state.bytes += bytes;
        state.peak = state.peak.max(state.bytes);
        Some(BytePermit {
            queue: Arc::clone(self),
            bytes,
        })
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }
    pub(crate) fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        self.ready.notify_all();
    }
    #[cfg(test)]
    pub(crate) fn usage(&self) -> (usize, usize) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.bytes, state.peak)
    }
}
#[derive(Debug)]
pub(crate) struct BytePermit {
    queue: Arc<ByteQueue>,
    bytes: usize,
}
impl Drop for BytePermit {
    fn drop(&mut self) {
        self.queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bytes -= self.bytes;
        self.queue.ready.notify_all();
    }
}
#[derive(Debug)]
pub(crate) struct CloseQueue(pub Arc<ByteQueue>);
impl Drop for CloseQueue {
    fn drop(&mut self) {
        self.0.close();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use cshell_terminal::TerminalEngine;
    #[test]
    fn byte_permits_backpressure_release_and_close_waiters() {
        let queue = Arc::new(ByteQueue::default());
        let full = queue.reserve(OUTPUT_QUEUE_BYTES, false).unwrap();
        assert!(queue.reserve(1, false).is_none());
        let other = queue.clone();
        let waiter = std::thread::spawn(move || other.reserve(1, true));
        queue.close();
        assert!(waiter.join().unwrap().is_none());
        drop(full);
        assert_eq!(queue.usage(), (0, OUTPUT_QUEUE_BYTES));
    }
    #[test]
    fn twenty_engines_share_budget_and_reclaim_idle_and_alternate_history() {
        let pool = HistoryPool::new(4 * 1024 * 1024);
        let mut engines = Vec::new();
        for _ in 0..20 {
            let engine = Arc::new(Mutex::new(AlacrittyTerminalEngine::new(
                cshell_domain::TerminalSize::cells(4, 80),
            )));
            pool.register(&engine);
            engine
                .lock()
                .unwrap()
                .feed("row\r\n".repeat(4000).as_bytes());
            engine.lock().unwrap().feed(b"\x1b[?1049h\x1b[HALT");
            engines.push(engine);
        }
        let total: usize = engines
            .iter()
            .map(|e| e.lock().unwrap().retained_history_bytes())
            .sum();
        assert!(total <= pool.limit);
        for e in engines {
            let mut e = e.lock().unwrap();
            assert!(e.snapshot().row(0).unwrap()[0].character == 'A');
            e.feed(b"\x1b[?1049l");
            assert!(e.retained_history_bytes() <= pool.limit / 20);
        }
    }
}

#[cfg(test)]
mod burst_tests {
    use super::*;
    #[test]
    fn burst_is_lossless_byte_bounded_and_publications_are_coalesced() {
        use std::sync::atomic::Ordering;
        let directory = tempfile::tempdir().unwrap();
        let pipeline = crate::TerminalPipeline::spawn_configured(
            directory.path().join("unused.csjr"),
            cshell_domain::TerminalSize::cells(4, 80),
            256,
            None,
            false,
        )
        .unwrap();
        let ingress = pipeline.ingress().unwrap();
        let snapshots = pipeline.snapshots();
        let started = std::time::Instant::now();
        let producer = std::thread::spawn(move || {
            for _ in 0..1000 {
                ingress.submit_blocking(vec![b'x'; 1024]).unwrap();
            }
            ingress
                .submit_blocking(b"\r\nBURST_DONE\r\n".to_vec())
                .unwrap();
        });
        producer.join().unwrap();
        let snapshot = snapshots
            .wait_for_generation(1001, std::time::Duration::from_secs(20))
            .unwrap();
        let elapsed = started.elapsed();
        let count = snapshots.publications.load(Ordering::Relaxed);
        assert!(count <= elapsed.as_millis() as u64 / 16 + 4);
        assert!(
            count < 500,
            "network chunks must not each publish a full snapshot"
        );
        let stats = pipeline.telemetry().snapshot();
        assert_eq!(stats.processed_output_bytes, stats.accepted_output_bytes);
        assert_eq!(stats.processed_output_bytes, 1000 * 1024 + 14);
        assert!(pipeline.bytes.usage().1 <= OUTPUT_QUEUE_BYTES);
        let text: String = (0..snapshot.rows)
            .flat_map(|row| {
                snapshot
                    .row(row)
                    .unwrap()
                    .iter()
                    .flat_map(cshell_terminal::Cell::characters)
            })
            .collect();
        assert!(text.contains("BURST_DONE"));
        pipeline.shutdown().unwrap();
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}

#[cfg(test)]
mod activity_tests {
    #[test]
    fn subscriptions_restore_interactive_rate_and_last_clone_drop_restores_background_rate() {
        let latest = crate::LatestSnapshot::default();
        assert_eq!(latest.publication_interval().as_millis(), 100);
        let view =
            crate::TerminalFrameSubscription::new(cshell_domain::SessionId::new(), latest.clone());
        let second = view.clone();
        assert_eq!(latest.publication_interval().as_millis(), 16);
        drop(view);
        assert_eq!(latest.publication_interval().as_millis(), 16);
        drop(second);
        assert_eq!(latest.publication_interval().as_millis(), 100);
    }
}

#[cfg(test)]
mod submission_error_tests {
    #[test]
    fn oversized_and_closed_submissions_are_not_retryable_backpressure() {
        let directory = tempfile::tempdir().unwrap();
        let pipeline = crate::TerminalPipeline::spawn_configured(
            directory.path().join("unused"),
            cshell_domain::TerminalSize::cells(4, 80),
            8,
            None,
            false,
        )
        .unwrap();
        assert!(matches!(
            pipeline.try_submit(vec![0; super::OUTPUT_QUEUE_BYTES + 1]),
            Err(crate::SubmitError::TooLarge(_))
        ));
        pipeline.bytes.close();
        assert!(matches!(
            pipeline.try_submit(b"text".to_vec()),
            Err(crate::SubmitError::Closed(_))
        ));
        pipeline.shutdown().unwrap();
    }
}

#[cfg(test)]
mod priority_tests {
    use super::*;
    #[test]
    fn visible_subscribers_get_priority_and_released_sessions_restore_capacity() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let pool = HistoryPool::new(2 * 1024 * 1024);
        let make = || {
            Arc::new(Mutex::new(AlacrittyTerminalEngine::new(
                cshell_domain::TerminalSize::cells(4, 80),
            )))
        };
        let first = make();
        let second = make();
        let active = Arc::new(AtomicUsize::new(0));
        let idle = Arc::new(AtomicUsize::new(0));
        pool.register_with_activity(&first, &active);
        pool.register_with_activity(&second, &idle);
        active.store(1, Ordering::Relaxed);
        pool.refresh();
        assert!(
            first.lock().unwrap().history_budget_bytes()
                > second.lock().unwrap().history_budget_bytes()
        );
        drop(second);
        pool.refresh();
        assert_eq!(first.lock().unwrap().history_budget_bytes(), pool.limit);
    }
}
