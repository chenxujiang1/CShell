//! Daemon-owned ordered terminal pipeline used by the Phase 0 vertical slice.

mod clipboard;
mod zmodem;
mod terminal_history;
mod terminal_resources;
mod clipboard_policy;
mod credential_package;
mod download_executor;
mod desktop_lifetime;
mod download_local;
mod download_service;
mod mixed_service;
pub use download_service::BatchDownloadService;
pub use mixed_service::{MixedRetryError, MixedTaskService};
mod host_group_ipc;
mod task_ipc;
mod task_output;
mod task_ssh;
mod upload_executor;
mod upload_service;
pub use task_ipc::TaskIpcService;
pub use task_output::{
    MAX_TASK_OUTPUT_PAGE, MAX_TASK_STEP_OUTPUT, TaskOutputError, TaskOutputStore,
};
pub use task_ssh::SshTaskExecutor;
pub use upload_service::BatchUploadService;
mod host_ca_ipc;
mod host_ca_store;
mod profile_batch_ipc;
mod profile_ipc;
mod quick_command_ipc;
mod script_control;
mod session_lock;
mod ssh_provider_ipc;
mod terminal_policy_ipc;
mod terminal_policy_store;
mod terminal_tools_ipc;
mod terminal_tools_store;
mod tunnel_ipc;
mod tunnel_store;
mod keyboard_auth;
mod shared_ssh;
mod vault_control;
mod vault_ipc;
pub use session_lock::SessionLockMonitor;

#[cfg(test)]
mod diagnostic_regressions {
    use std::error::Error;
    #[test]
    fn local_and_storage_errors_output_queues_and_journals_hide_sensitive_values() {
        let errors: Vec<Box<dyn Error>> = vec![
            Box::new(cshell_storage::StorageError::Sqlite(sqlx::Error::Protocol(
                "SENTINEL".into(),
            ))),
            Box::new(cshell_storage::StorageError::Io(std::io::Error::other(
                "SENTINEL",
            ))),
            Box::new(cshell_local::LocalPtyError::Open("SENTINEL".into())),
            Box::new(cshell_local::LocalPtyError::Spawn("SENTINEL".into())),
            Box::new(cshell_local::LocalPtyError::Resize("SENTINEL".into())),
            Box::new(cshell_local::LocalPtyError::ProcessTree(
                std::io::Error::other("SENTINEL"),
            )),
            Box::new(crate::LocalSessionError::OutputRead(std::io::Error::other(
                "SENTINEL",
            ))),
            Box::new(cshell_output_store::JournalError::CorruptSegment {
                segment_id: 1,
                detail: "SENTINEL".into(),
            }),
            Box::new(cshell_output_store::JournalError::PostCommitIndex {
                sequence: 2,
                source: Box::new(cshell_output_store::JournalError::Io(
                    std::io::Error::other("SENTINEL"),
                )),
            }),
            Box::new(cshell_output_store::JournalError::JournalTailRollback {
                record_start: 3,
                write_error: "SENTINEL".into(),
                rollback_error: "SENTINEL".into(),
            }),
        ];
        for error in errors {
            assert!(!format!("{error} {error:?}").contains("SENTINEL"));
            assert!(error.source().is_none());
        }
        let output = crate::SubmitError::Closed(b"SENTINEL".to_vec());
        let record = cshell_output_store::JournalRecord {
            sequence: 1,
            payload: b"SENTINEL".to_vec(),
        };
        assert!(!format!("{output:?} {record:?}").contains("83, 69"));
        let mut profile = cshell_local::LocalProfile::platform_default();
        profile.args = vec!["SENTINEL".into()];
        profile
            .env_overrides
            .insert("TOKEN".into(), "SENTINEL".into());
        assert!(!format!("{profile:?}").contains("SENTINEL"));
    }
}

mod exit_monitor;
mod frame_subscription;
mod ipc_server;
mod local_session;
mod session_ipc;
mod session_registry;
mod sftp_ipc;
mod ssh_route;
mod ssh_session;

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
pub use ssh_session::{SshSession, SshSessionError, SshSessionRegistry};

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

pub enum SubmitError {
    Backpressure(Vec<u8>),
    TooLarge(Vec<u8>),
    Closed(Vec<u8>),
}
impl std::fmt::Debug for SubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Backpressure(bytes) => f.debug_tuple("Backpressure").field(&bytes.len()).finish(),
            Self::TooLarge(bytes) => f.debug_tuple("TooLarge").field(&bytes.len()).finish(),
            Self::Closed(bytes) => f.debug_tuple("Closed").field(&bytes.len()).finish(),
        }
    }
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
    Output(Vec<u8>, terminal_resources::BytePermit),
    Resize(TerminalSize),
    ResetConnection(TerminalSize, mpsc::SyncSender<u64>),
}

#[derive(Clone, Debug)]
pub struct PipelineIngress {
    sender: Weak<SyncSender<PipelineMessage>>,
    telemetry: PipelineTelemetry,
    bytes: Arc<terminal_resources::ByteQueue>,
}

impl PipelineIngress {
    fn closed() -> Self {
        Self {
            sender: Weak::new(),
            telemetry: PipelineTelemetry::default(),
            bytes: Arc::new(terminal_resources::ByteQueue::default()),
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
        for (index, chunk) in bytes.chunks(terminal_resources::OUTPUT_CHUNK_BYTES).enumerate() {
            let Some(permit) = self.bytes.reserve(chunk.len(), true) else {
                return Err(SubmitError::Closed(bytes[index * terminal_resources::OUTPUT_CHUNK_BYTES..].to_vec()));
            };
            self.telemetry.begin_output(chunk.len());
            if sender.send(PipelineMessage::Output(chunk.to_vec(), permit)).is_err() {
                self.telemetry.cancel_output(chunk.len());
                return Err(SubmitError::Closed(bytes[index * terminal_resources::OUTPUT_CHUNK_BYTES..].to_vec()));
            }
        }
        Ok(())
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

    /// Fence the previous transport's parser work before exposing a new shell.
    pub(crate) fn reset_connection_blocking(&self, size: TerminalSize) -> Result<u64, PipelineClosed> {
        let sender = self.sender.upgrade().ok_or(PipelineClosed)?;
        let (done, receipt) = mpsc::sync_channel(1);
        self.telemetry.begin_resize();
        sender.send(PipelineMessage::ResetConnection(size, done)).map_err(|_| {
            self.telemetry.cancel_resize();
            PipelineClosed
        })?;
        receipt.recv_timeout(Duration::from_secs(5)).map_err(|_| PipelineClosed)
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
    terminal: Option<Arc<Mutex<AlacrittyTerminalEngine>>>,
    subscribers: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    publications: Arc<std::sync::atomic::AtomicU64>,
}

#[derive(Debug)]
pub(crate) struct SnapshotActivity(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for SnapshotActivity {
    fn drop(&mut self) { self.0.fetch_sub(1, Ordering::Relaxed); }
}
impl LatestSnapshot {
    pub(crate) fn shell_history(&self, session:cshell_domain::SessionId, id:u64)->Option<cshell_domain::ShellHistory> {
        self.terminal.as_ref().map(|engine|engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner).shell_history(session,id))
    }
    pub(crate) fn activity(&self) -> Arc<SnapshotActivity> {
        self.subscribers.fetch_add(1, Ordering::Relaxed);
        Arc::new(SnapshotActivity(Arc::clone(&self.subscribers)))
    }
    fn publication_interval(&self) -> Duration {
        Duration::from_millis(if self.subscribers.load(Ordering::Relaxed)>0 {16} else {100})
    }
    pub(crate) fn history_page(&self,request:&cshell_ipc::LogPageRequest)->Option<cshell_ipc::LogPage> {
        self.terminal.as_ref().map(|engine| {
            let slice = engine.lock().unwrap_or_else(std::sync::PoisonError::into_inner).history_slice(
                request.anchor_line_id, request.rows_before as usize, request.rows_after as usize);
            crate::terminal_history::page(slice, request)
        })
    }

    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
    fn publish(&self, snapshot: FrameSnapshot) {
        #[cfg(test)]
        self.publications.fetch_add(1, Ordering::Relaxed);
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
    bytes: Arc<terminal_resources::ByteQueue>,
    worker: Option<JoinHandle<Result<(), PipelineError>>>,
}

impl TerminalPipeline {
    pub fn spawn(
        journal_path: impl AsRef<Path>,
        terminal_size: TerminalSize,
        ingress_capacity: usize,
    ) -> Result<Self, PipelineError> {
        Self::spawn_inner(journal_path, terminal_size, ingress_capacity, None, true)
    }

    pub(crate) fn spawn_with_clipboard(
        journal_path: impl AsRef<Path>,
        terminal_size: TerminalSize,
        ingress_capacity: usize,
        clipboard: clipboard::ClipboardInbox,
    ) -> Result<Self, PipelineError> {
        Self::spawn_inner(
            journal_path,
            terminal_size,
            ingress_capacity,
            Some(clipboard),
            true,
        )
    }

    pub(crate) fn spawn_configured(
        journal_path: impl AsRef<Path>,
        terminal_size: TerminalSize,
        ingress_capacity: usize,
        clipboard: Option<clipboard::ClipboardInbox>,
        logging: bool,
    ) -> Result<Self, PipelineError> {
        Self::spawn_inner(
            journal_path,
            terminal_size,
            ingress_capacity,
            clipboard,
            logging,
        )
    }

    fn spawn_inner(
        journal_path: impl AsRef<Path>,
        terminal_size: TerminalSize,
        ingress_capacity: usize,
        clipboard: Option<clipboard::ClipboardInbox>,
        logging: bool,
    ) -> Result<Self, PipelineError> {
        let journal_path = journal_path.as_ref();
        let mut journal = logging
            .then(|| {
                SegmentedJournalWriter::create(
                    journal_path,
                    Durability::SessionLog,
                    DEFAULT_SEGMENT_BYTES,
                )
            })
            .transpose()?;
        let line_index = journal.as_ref().map_or_else(
            || JournalLineIndex::recording_disabled(journal_path.to_path_buf()),
            SegmentedJournalWriter::line_index,
        );
        let mut timing = logging
            .then(|| {
                cshell_output_store::JournalTimingWriter::create(
                    journal_path,
                    line_index.record_count(),
                )
            })
            .transpose()?;
        let mut capture_size = terminal_size;
        let terminal = Arc::new(Mutex::new(AlacrittyTerminalEngine::new(terminal_size)));
        let latest = LatestSnapshot {terminal:Some(Arc::clone(&terminal)),..Default::default()};
        terminal_resources::register_history(&terminal, &latest.subscribers);
        latest.publish(terminal.lock().unwrap_or_else(std::sync::PoisonError::into_inner).snapshot());
        let worker_latest = latest.clone();
        let responses = TerminalResponses::new(DEFAULT_RESPONSE_CAPACITY);
        let worker_responses = responses.clone();
        let telemetry = PipelineTelemetry::default();
        let worker_telemetry = telemetry.clone();
        let byte_queue = Arc::new(terminal_resources::ByteQueue::default());
        let worker_byte_queue = Arc::clone(&byte_queue);
        let (sender, receiver) = mpsc::sync_channel::<PipelineMessage>(ingress_capacity.max(1));
        let sender = Arc::new(sender);
        let worker = thread::Builder::new()
            .name("cshell-terminal-parser".to_owned())
            .spawn(move || {
                let mut observer = clipboard
                    .as_ref()
                    .map(|_| clipboard::ClipboardObserver::default());
                let _close_queue = terminal_resources::CloseQueue(worker_byte_queue);
                let mut publish_at = Instant::now();
                let mut dirty = false;
                loop {
                    let interval = worker_latest.publication_interval();
                    publish_at = publish_at.min(Instant::now() + interval);
                    let message = match receiver.recv_timeout(if dirty {publish_at.saturating_duration_since(Instant::now())} else {Duration::from_millis(100)}) {
                        Ok(message) => message,
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if dirty {
                                worker_latest.publish(terminal.lock().unwrap_or_else(std::sync::PoisonError::into_inner).snapshot());
                                dirty = false; publish_at = Instant::now() + interval;
                            }
                            continue;
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            if dirty {worker_latest.publish(terminal.lock().unwrap_or_else(std::sync::PoisonError::into_inner).snapshot());}
                            break;
                        }
                    };
                    terminal_resources::refresh_history();
                    let mut terminal=terminal.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    match message {
                        PipelineMessage::Output(bytes, _permit) => {
                            let byte_count = bytes.len();
                            if let (Some(journal), Some(timing)) = (&mut journal, &mut timing) {
                                let sequence = journal.line_index().record_count();
                                timing.append(sequence, capture_size.rows, capture_size.cols)?;
                                journal.append(&bytes)?;
                            }
                            if let (Some(inbox), Some(observer)) = (&clipboard, &mut observer) {
                                observer.feed(&bytes, inbox);
                            }
                            let delta = terminal.feed(&bytes);
                            worker_responses.publish(delta.terminal_responses)?;
                            dirty = true;
                            if Instant::now() >= publish_at {
                                worker_latest.publish(terminal.snapshot());
                                dirty = false; publish_at = Instant::now() + interval;
                            }
                            worker_telemetry.finish_output(byte_count);
                        }
                        PipelineMessage::Resize(size) => {
                            capture_size = size;
                            terminal.resize(size);
                            worker_latest.publish(terminal.snapshot());
                            dirty = false; publish_at = Instant::now() + interval;
                            worker_telemetry.finish_resize();
                        }
                        PipelineMessage::ResetConnection(size, done) => {
                            capture_size = size;
                            if let (Some(journal), Some(timing)) = (&mut journal, &mut timing) {
                                timing.append(journal.line_index().record_count(), size.rows, size.cols)?;
                                journal.append(b"\x1bc")?;
                            }
                            terminal.reset_connection(size);
                            if observer.is_some() { observer = Some(clipboard::ClipboardObserver::default()); }
                            let _ = worker_responses.drain();
                            let snapshot = terminal.snapshot();
                            let generation = snapshot.generation;
                            worker_latest.publish(snapshot);
                            dirty = false; publish_at = Instant::now() + interval;
                            worker_telemetry.finish_resize();
                            let _ = done.send(generation);
                        }
                    }
                }
                if let Some(journal) = &mut journal {
                    journal.flush()?;
                }
                if let Some(timing) = &mut timing {
                    timing.flush()?;
                }
                Ok(())
            })
            .map_err(JournalError::Io)?;
        Ok(Self {
            ingress: Some(sender),
            latest,
            responses,
            line_index,
            telemetry,
            bytes: byte_queue,
            worker: Some(worker),
        })
    }

    pub fn try_submit(&self, bytes: Vec<u8>) -> Result<(), SubmitError> {
        let Some(ingress) = self.ingress.as_ref() else {
            return Err(SubmitError::Closed(bytes));
        };
        if bytes.len() > terminal_resources::OUTPUT_QUEUE_BYTES { return Err(SubmitError::TooLarge(bytes)); }
        let Some(permit) = self.bytes.reserve(bytes.len(), false) else {
            return Err(if self.bytes.is_closed() {SubmitError::Closed(bytes)} else {SubmitError::Backpressure(bytes)});
        };
        let byte_count = bytes.len();
        self.telemetry.begin_output(byte_count);
        ingress
            .try_send(PipelineMessage::Output(bytes, permit))
            .map_err(|error| {
                self.telemetry.cancel_output(byte_count);
                match error {
                    TrySendError::Full(PipelineMessage::Output(bytes, _permit)) => {
                        SubmitError::Backpressure(bytes)
                    }
                    TrySendError::Disconnected(PipelineMessage::Output(bytes, _permit)) => {
                        SubmitError::Closed(bytes)
                    }
                    TrySendError::Full(PipelineMessage::Resize(_))
                    | TrySendError::Disconnected(PipelineMessage::Resize(_))
                    | TrySendError::Full(PipelineMessage::ResetConnection(..))
                    | TrySendError::Disconnected(PipelineMessage::ResetConnection(..)) => {
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
            bytes: Arc::clone(&self.bytes),
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
    fn connection_reset_fences_old_responses_modes_and_partial_escape_sequences() {
        let directory = tempfile::tempdir().unwrap();
        let pipeline = TerminalPipeline::spawn(directory.path().join("reset.csjr"), TerminalSize::cells(24, 80), 8).unwrap();
        let ingress = pipeline.ingress().unwrap();
        ingress.submit_blocking(b"\x1b[?2004h\x1b[>4;2m\x1b[6n\x1b]0;unfinished".to_vec()).unwrap();
        let before = pipeline.snapshots().wait_for_generation(1, Duration::from_secs(2)).unwrap();
        assert!(!pipeline.responses().is_empty());
        let size = TerminalSize::cells(30, 100);
        ingress.reset_connection_blocking(size).unwrap();
        let reset = pipeline.snapshots().latest().unwrap();
        assert!(reset.generation > before.generation);
        assert_eq!(reset.terminal_modes, cshell_terminal::TerminalModes::default());
        assert!(pipeline.responses().is_empty());
        ingress.submit_blocking(b"fresh shell".to_vec()).unwrap();
        let fresh = pipeline.snapshots().wait_for_generation(reset.generation + 1, Duration::from_secs(2)).unwrap();
        assert_eq!(fresh.row(0).unwrap()[0].character, 'f');
        pipeline.shutdown().unwrap();
    }

    #[test]
    fn clipboard_observation_never_replies_to_remote_reads() {
        use cshell_ipc::ClipboardOperation;
        let directory = tempfile::tempdir().unwrap();
        let inbox = crate::clipboard::ClipboardInbox::default();
        let pipeline = TerminalPipeline::spawn_with_clipboard(
            directory.path().join("clipboard.csjr"),
            TerminalSize::cells(2, 20),
            8,
            inbox.clone(),
        )
        .unwrap();
        let snapshots = pipeline.snapshots();
        pipeline
            .try_submit(b"\x1b]52;c;?\x07\x1b]52;c;aGVsbG8=\x07visible".to_vec())
            .unwrap();
        let snapshot = snapshots
            .wait_for_generation(1, Duration::from_secs(2))
            .unwrap();
        assert!(pipeline.responses().is_empty());
        assert_eq!(snapshot.row(0).unwrap()[0].character, 'v');
        let poll = inbox.handle(ClipboardOperation::Poll, &[], true);
        assert_eq!(poll.byte_count, 5);
        assert!(poll.text.is_empty());
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

mod sftp_lifecycle;
#[cfg(test)]
mod test_resources;
