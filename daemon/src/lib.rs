//! Daemon-owned ordered terminal pipeline used by the Phase 0 vertical slice.

mod profile_ipc;

mod exit_monitor;
mod frame_subscription;
mod ipc_server;
mod local_session;
mod session_ipc;
mod session_registry;

pub use cshell_vault::{KeychainError, ProbeId, Secret, SystemKeychain};
pub use exit_monitor::{SessionExitMonitor, SessionExitMonitorError};
pub use frame_subscription::{SubscriptionFrame, SubscriptionStats, TerminalFrameSubscription};
pub use ipc_server::{IpcServerError, IpcServerStats, SessionIpcServer};
pub use local_session::{
    LocalSessionError, LocalSessionExit, LocalTerminalHandle, LocalTerminalSession,
};
pub use profile_ipc::ProfileIpcService;
pub use session_ipc::{SessionIpcError, SessionIpcService};
pub use session_registry::{
    LocalSessionAttachment, LocalSessionInfo, LocalSessionRegistry, SessionRegistryError,
};

use cshell_domain::TerminalSize;
use cshell_output_store::{
    DEFAULT_SEGMENT_BYTES, Durability, JournalError, JournalLineIndex, SegmentedJournalWriter,
};
use cshell_terminal::{AlacrittyTerminalEngine, FrameSnapshot, TerminalEngine};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug)]
pub enum SubmitError {
    Backpressure(Vec<u8>),
    Closed(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("terminal pipeline is closed")]
pub struct PipelineClosed;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PipelineStats {
    pub accepted_output_bytes: u64,
    pub processed_output_bytes: u64,
    pub accepted_output_messages: u64,
    pub processed_output_messages: u64,
    pub accepted_resizes: u64,
    pub processed_resizes: u64,
    pub in_flight_messages: usize,
    pub peak_in_flight_messages: usize,
}

#[derive(Debug, Default)]
struct PipelineTelemetryState {
    accepted_output_bytes: AtomicU64,
    processed_output_bytes: AtomicU64,
    accepted_output_messages: AtomicU64,
    processed_output_messages: AtomicU64,
    accepted_resizes: AtomicU64,
    processed_resizes: AtomicU64,
    in_flight_messages: AtomicUsize,
    peak_in_flight_messages: AtomicUsize,
}

#[derive(Clone, Debug, Default)]
pub struct PipelineTelemetry {
    shared: Arc<PipelineTelemetryState>,
}

impl PipelineTelemetry {
    fn begin_message(&self) {
        let in_flight = self
            .shared
            .in_flight_messages
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        self.shared
            .peak_in_flight_messages
            .fetch_max(in_flight, Ordering::Relaxed);
    }

    fn finish_message(&self) {
        self.shared
            .in_flight_messages
            .fetch_sub(1, Ordering::Relaxed);
    }

    fn begin_output(&self, bytes: usize) {
        self.shared
            .accepted_output_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.shared
            .accepted_output_messages
            .fetch_add(1, Ordering::Relaxed);
        self.begin_message();
    }

    fn cancel_output(&self, bytes: usize) {
        self.shared
            .accepted_output_bytes
            .fetch_sub(bytes as u64, Ordering::Relaxed);
        self.shared
            .accepted_output_messages
            .fetch_sub(1, Ordering::Relaxed);
        self.finish_message();
    }

    fn finish_output(&self, bytes: usize) {
        self.shared
            .processed_output_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
        self.shared
            .processed_output_messages
            .fetch_add(1, Ordering::Relaxed);
        self.finish_message();
    }

    fn begin_resize(&self) {
        self.shared.accepted_resizes.fetch_add(1, Ordering::Relaxed);
        self.begin_message();
    }

    fn cancel_resize(&self) {
        self.shared.accepted_resizes.fetch_sub(1, Ordering::Relaxed);
        self.finish_message();
    }

    fn finish_resize(&self) {
        self.shared
            .processed_resizes
            .fetch_add(1, Ordering::Relaxed);
        self.finish_message();
    }

    #[must_use]
    pub fn snapshot(&self) -> PipelineStats {
        PipelineStats {
            accepted_output_bytes: self.shared.accepted_output_bytes.load(Ordering::Relaxed),
            processed_output_bytes: self.shared.processed_output_bytes.load(Ordering::Relaxed),
            accepted_output_messages: self.shared.accepted_output_messages.load(Ordering::Relaxed),
            processed_output_messages: self
                .shared
                .processed_output_messages
                .load(Ordering::Relaxed),
            accepted_resizes: self.shared.accepted_resizes.load(Ordering::Relaxed),
            processed_resizes: self.shared.processed_resizes.load(Ordering::Relaxed),
            in_flight_messages: self.shared.in_flight_messages.load(Ordering::Relaxed),
            peak_in_flight_messages: self.shared.peak_in_flight_messages.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
enum PipelineMessage {
    Output(Vec<u8>),
    Resize(TerminalSize),
}

#[derive(Clone, Debug)]
pub struct PipelineIngress {
    sender: Weak<SyncSender<PipelineMessage>>,
    telemetry: PipelineTelemetry,
}

impl PipelineIngress {
    fn closed() -> Self {
        Self {
            sender: Weak::new(),
            telemetry: PipelineTelemetry::default(),
        }
    }

    /// Submits lossless transport output, waiting for bounded parser capacity.
    ///
    /// Transport reader threads should use this method so that overload applies
    /// backpressure instead of silently dropping terminal bytes.
    pub fn submit_blocking(&self, bytes: Vec<u8>) -> Result<(), SubmitError> {
        let Some(sender) = self.sender.upgrade() else {
            return Err(SubmitError::Closed(bytes));
        };
        let byte_count = bytes.len();
        self.telemetry.begin_output(byte_count);
        sender
            .send(PipelineMessage::Output(bytes))
            .map_err(|error| {
                self.telemetry.cancel_output(byte_count);
                match error.0 {
                    PipelineMessage::Output(bytes) => SubmitError::Closed(bytes),
                    PipelineMessage::Resize(_) => unreachable!("submitted an output message"),
                }
            })
    }

    /// Serializes a resize behind all output already accepted by the parser.
    pub fn resize_blocking(&self, size: TerminalSize) -> Result<(), PipelineClosed> {
        let sender = self.sender.upgrade().ok_or(PipelineClosed)?;
        self.telemetry.begin_resize();
        sender.send(PipelineMessage::Resize(size)).map_err(|_| {
            self.telemetry.cancel_resize();
            PipelineClosed
        })
    }
}

#[derive(Debug, Error)]
pub enum PipelineError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("terminal pipeline worker panicked")]
    WorkerPanicked,
    #[error("terminal response queue reached its bounded capacity of {capacity}")]
    ResponseQueueFull { capacity: usize },
}

const DEFAULT_RESPONSE_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub struct TerminalResponses {
    shared: Arc<(Mutex<VecDeque<Vec<u8>>>, Condvar)>,
    capacity: usize,
}

impl TerminalResponses {
    fn new(capacity: usize) -> Self {
        Self {
            shared: Arc::new((
                Mutex::new(VecDeque::with_capacity(capacity)),
                Condvar::new(),
            )),
            capacity,
        }
    }

    fn publish(&self, responses: Vec<Vec<u8>>) -> Result<(), PipelineError> {
        if responses.is_empty() {
            return Ok(());
        }
        let (queue, signal) = &*self.shared;
        let mut queue = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if responses.len() > self.capacity.saturating_sub(queue.len()) {
            return Err(PipelineError::ResponseQueueFull {
                capacity: self.capacity,
            });
        }
        queue.extend(responses);
        signal.notify_all();
        Ok(())
    }

    /// Drains terminal-generated control responses in parser order.
    ///
    /// The transport owner must write these bytes before ordinary user input.
    #[must_use]
    pub fn drain(&self) -> Vec<Vec<u8>> {
        let (queue, _) = &*self.shared;
        let mut queue = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.drain(..).collect()
    }

    /// Waits for parser-generated control responses and drains them in order.
    #[must_use]
    pub fn wait_and_drain(&self, timeout: Duration) -> Vec<Vec<u8>> {
        let (queue, signal) = &*self.shared;
        let mut guard = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.is_empty() {
            guard = match signal.wait_timeout(guard, timeout) {
                Ok((guard, _)) => guard,
                Err(poisoned) => poisoned.into_inner().0,
            };
        }
        guard.drain(..).collect()
    }

    fn wake_waiters(&self) {
        self.shared.1.notify_all();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.shared
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct LatestSnapshot {
    shared: Arc<(Mutex<Option<FrameSnapshot>>, Condvar)>,
}

impl LatestSnapshot {
    fn publish(&self, snapshot: FrameSnapshot) {
        let (slot, signal) = &*self.shared;
        let mut guard = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(snapshot);
        signal.notify_all();
    }

    #[must_use]
    pub fn latest(&self) -> Option<FrameSnapshot> {
        let (slot, _) = &*self.shared;
        slot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    #[must_use]
    pub fn wait_for_generation(
        &self,
        minimum_generation: u64,
        timeout: Duration,
    ) -> Option<FrameSnapshot> {
        let (slot, signal) = &*self.shared;
        let mut guard = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = Instant::now() + timeout;
        loop {
            if guard
                .as_ref()
                .is_some_and(|snapshot| snapshot.generation >= minimum_generation)
            {
                return guard.clone();
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            let result = signal.wait_timeout(guard, remaining);
            match result {
                Ok((next_guard, wait_result)) => {
                    guard = next_guard;
                    if wait_result.timed_out() {
                        return guard
                            .clone()
                            .filter(|snapshot| snapshot.generation >= minimum_generation);
                    }
                }
                Err(poisoned) => {
                    let (next_guard, _) = poisoned.into_inner();
                    guard = next_guard;
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct TerminalPipeline {
    ingress: Option<Arc<SyncSender<PipelineMessage>>>,
    latest: LatestSnapshot,
    responses: TerminalResponses,
    line_index: JournalLineIndex,
    telemetry: PipelineTelemetry,
    worker: Option<JoinHandle<Result<(), PipelineError>>>,
}

impl TerminalPipeline {
    pub fn spawn(
        journal_path: impl AsRef<Path>,
        terminal_size: TerminalSize,
        ingress_capacity: usize,
    ) -> Result<Self, PipelineError> {
        let mut journal = SegmentedJournalWriter::create(
            journal_path,
            Durability::SessionLog,
            DEFAULT_SEGMENT_BYTES,
        )?;
        let line_index = journal.line_index();
        let mut terminal = AlacrittyTerminalEngine::new(terminal_size);
        let latest = LatestSnapshot::default();
        latest.publish(terminal.snapshot());
        let worker_latest = latest.clone();
        let responses = TerminalResponses::new(DEFAULT_RESPONSE_CAPACITY);
        let worker_responses = responses.clone();
        let telemetry = PipelineTelemetry::default();
        let worker_telemetry = telemetry.clone();
        let (sender, receiver) = mpsc::sync_channel::<PipelineMessage>(ingress_capacity.max(1));
        let sender = Arc::new(sender);
        let worker = thread::Builder::new()
            .name("cshell-terminal-parser".to_owned())
            .spawn(move || {
                while let Ok(message) = receiver.recv() {
                    match message {
                        PipelineMessage::Output(bytes) => {
                            let byte_count = bytes.len();
                            journal.append(&bytes)?;
                            let delta = terminal.feed(&bytes);
                            worker_responses.publish(delta.terminal_responses)?;
                            worker_latest.publish(terminal.snapshot());
                            worker_telemetry.finish_output(byte_count);
                        }
                        PipelineMessage::Resize(size) => {
                            terminal.resize(size);
                            worker_latest.publish(terminal.snapshot());
                            worker_telemetry.finish_resize();
                        }
                    }
                }
                journal.flush()?;
                Ok(())
            })
            .map_err(JournalError::Io)?;
        Ok(Self {
            ingress: Some(sender),
            latest,
            responses,
            line_index,
            telemetry,
            worker: Some(worker),
        })
    }

    pub fn try_submit(&self, bytes: Vec<u8>) -> Result<(), SubmitError> {
        let Some(ingress) = self.ingress.as_ref() else {
            return Err(SubmitError::Closed(bytes));
        };
        let byte_count = bytes.len();
        self.telemetry.begin_output(byte_count);
        ingress
            .try_send(PipelineMessage::Output(bytes))
            .map_err(|error| {
                self.telemetry.cancel_output(byte_count);
                match error {
                    TrySendError::Full(PipelineMessage::Output(bytes)) => {
                        SubmitError::Backpressure(bytes)
                    }
                    TrySendError::Disconnected(PipelineMessage::Output(bytes)) => {
                        SubmitError::Closed(bytes)
                    }
                    TrySendError::Full(PipelineMessage::Resize(_))
                    | TrySendError::Disconnected(PipelineMessage::Resize(_)) => {
                        unreachable!("submitted an output message")
                    }
                }
            })
    }

    #[must_use]
    pub fn ingress(&self) -> Option<PipelineIngress> {
        self.ingress.as_ref().map(|sender| PipelineIngress {
            sender: Arc::downgrade(sender),
            telemetry: self.telemetry.clone(),
        })
    }

    #[must_use]
    pub fn snapshots(&self) -> LatestSnapshot {
        self.latest.clone()
    }

    #[must_use]
    pub fn responses(&self) -> TerminalResponses {
        self.responses.clone()
    }

    #[must_use]
    pub fn line_index(&self) -> JournalLineIndex {
        self.line_index.clone()
    }

    #[must_use]
    pub fn telemetry(&self) -> PipelineTelemetry {
        self.telemetry.clone()
    }

    pub fn shutdown(mut self) -> Result<(), PipelineError> {
        self.ingress.take();
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker.join().map_err(|_| PipelineError::WorkerPanicked)?
    }
}

impl Drop for TerminalPipeline {
    fn drop(&mut self) {
        self.ingress.take();
        if let Some(worker) = self.worker.take() {
            let _result = worker.join();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{PipelineError, TerminalPipeline};
    use cshell_domain::TerminalSize;
    use cshell_output_store::scan;
    use std::time::Duration;

    #[test]
    fn bytes_are_journaled_and_published_as_latest_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pipeline.csjr");
        let pipeline = TerminalPipeline::spawn(&path, TerminalSize::cells(24, 80), 8).unwrap();
        let snapshots = pipeline.snapshots();
        let telemetry = pipeline.telemetry();
        pipeline.try_submit(b"hello\r\n".to_vec()).unwrap();
        let snapshot = snapshots
            .wait_for_generation(1, Duration::from_secs(2))
            .unwrap();
        assert_eq!(snapshot.row(0).unwrap()[0].character, 'h');
        pipeline.shutdown().unwrap();
        let stats = telemetry.snapshot();
        assert_eq!(stats.accepted_output_bytes, 7);
        assert_eq!(stats.processed_output_bytes, 7);
        assert_eq!(stats.accepted_output_messages, 1);
        assert_eq!(stats.processed_output_messages, 1);
        assert_eq!(stats.in_flight_messages, 0);
        assert_eq!(stats.peak_in_flight_messages, 1);

        let report = scan(path, false).unwrap();
        assert_eq!(report.records.len(), 1);
        assert_eq!(report.records[0].payload, b"hello\r\n");
    }

    #[test]
    fn terminal_control_responses_are_ordered_and_drainable() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("responses.csjr");
        let pipeline = TerminalPipeline::spawn(&path, TerminalSize::cells(24, 80), 8).unwrap();
        let snapshots = pipeline.snapshots();
        let responses = pipeline.responses();
        pipeline.try_submit(b"abc\x1b[6n".to_vec()).unwrap();
        snapshots
            .wait_for_generation(1, Duration::from_secs(2))
            .unwrap();
        assert_eq!(responses.drain(), vec![b"\x1b[1;4R".to_vec()]);
        assert!(responses.is_empty());
        pipeline.shutdown().unwrap();
    }

    #[test]
    fn response_queue_overflow_fails_explicitly() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("response-overflow.csjr");
        let pipeline = TerminalPipeline::spawn(&path, TerminalSize::cells(24, 80), 8).unwrap();
        let snapshots = pipeline.snapshots();
        for generation in 1..=64 {
            pipeline.try_submit(b"\x1b[6n".to_vec()).unwrap();
            snapshots
                .wait_for_generation(generation, Duration::from_secs(2))
                .unwrap();
        }
        pipeline.try_submit(b"\x1b[6n".to_vec()).unwrap();
        assert!(matches!(
            pipeline.shutdown(),
            Err(PipelineError::ResponseQueueFull { capacity: 64 })
        ));
    }

    #[test]
    fn resize_is_serialized_through_the_parser_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("resize.csjr");
        let pipeline = TerminalPipeline::spawn(&path, TerminalSize::cells(24, 80), 8).unwrap();
        let snapshots = pipeline.snapshots();
        let telemetry = pipeline.telemetry();
        pipeline
            .ingress()
            .unwrap()
            .resize_blocking(TerminalSize::cells(40, 120))
            .unwrap();

        let snapshot = snapshots
            .wait_for_generation(1, Duration::from_secs(2))
            .unwrap();
        assert_eq!((snapshot.rows, snapshot.cols), (40, 120));
        pipeline.shutdown().unwrap();
        let stats = telemetry.snapshot();
        assert_eq!(stats.accepted_resizes, 1);
        assert_eq!(stats.processed_resizes, 1);
        assert_eq!(stats.in_flight_messages, 0);
    }
}
