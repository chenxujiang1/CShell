use crate::{
    LatestSnapshot, PipelineClosed, PipelineError, PipelineIngress, SubmitError, TerminalPipeline,
    TerminalResponses,
};
use cshell_domain::{InputAction, TerminalSize};
use cshell_local::{LocalProfile, LocalPtyError, PtySession};
use cshell_output_store::JournalLineIndex;
use cshell_terminal::{InputEncodeError, InputEncoder, TerminalModes};
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use thiserror::Error;

const CONTROL_QUEUE_CAPACITY: usize = 64;
const USER_QUEUE_CAPACITY: usize = 256;
const OUTPUT_READ_BUFFER_SIZE: usize = 64 * 1024;
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSessionExit {
    pub success: bool,
    pub code: u32,
    pub signal: Option<String>,
}

#[derive(Debug, Error)]
pub enum LocalSessionError {
    #[error(transparent)]
    Pty(#[from] LocalPtyError),
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
    #[error(transparent)]
    Input(#[from] InputEncodeError),
    #[error(transparent)]
    PipelineClosed(#[from] PipelineClosed),
    #[error("local PTY output reader failed: {0}")]
    OutputRead(std::io::Error),
    #[error("local terminal input queue is full")]
    InputBackpressure,
    #[error("local terminal session is closed")]
    Closed,
    #[error("local terminal {0} worker panicked")]
    WorkerPanicked(&'static str),
    #[error("local terminal process did not exit within {0:?}")]
    ExitTimeout(Duration),
}

#[derive(Debug)]
struct OutboundState {
    control: VecDeque<Vec<u8>>,
    user: VecDeque<Vec<u8>>,
    closed: bool,
}

#[derive(Clone, Debug)]
struct OutboundQueue {
    shared: Arc<(Mutex<OutboundState>, Condvar)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutboundError {
    Full,
    Closed,
}

impl OutboundQueue {
    fn new() -> Self {
        Self {
            shared: Arc::new((
                Mutex::new(OutboundState {
                    control: VecDeque::with_capacity(CONTROL_QUEUE_CAPACITY),
                    user: VecDeque::with_capacity(USER_QUEUE_CAPACITY),
                    closed: false,
                }),
                Condvar::new(),
            )),
        }
    }

    fn try_submit_user(&self, bytes: Vec<u8>) -> Result<(), OutboundError> {
        let (state, signal) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(OutboundError::Closed);
        }
        if state.user.len() == USER_QUEUE_CAPACITY {
            return Err(OutboundError::Full);
        }
        state.user.push_back(bytes);
        signal.notify_one();
        Ok(())
    }

    fn submit_control_blocking(&self, bytes: Vec<u8>) -> Result<(), OutboundError> {
        let (state, signal) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.closed && state.control.len() == CONTROL_QUEUE_CAPACITY {
            state = signal
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.closed {
            return Err(OutboundError::Closed);
        }
        state.control.push_back(bytes);
        signal.notify_one();
        Ok(())
    }

    fn next_blocking(&self) -> Option<Vec<u8>> {
        let (state, signal) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.closed && state.control.is_empty() && state.user.is_empty() {
            state = signal
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let bytes = state.control.pop_front().or_else(|| state.user.pop_front());
        signal.notify_all();
        bytes
    }

    fn close(&self) {
        let (state, signal) = &*self.shared;
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        signal.notify_all();
    }
}

/// Daemon-owned local terminal whose PTY survives independently of any GUI view.
///
/// PTY output is losslessly backpressured into the journal/parser pipeline. Parser
/// control responses use a separate queue that is always drained before user input.
#[derive(Debug)]
pub struct LocalTerminalSession {
    pty: Option<Arc<Mutex<PtySession>>>,
    pipeline: Option<TerminalPipeline>,
    ingress: Option<PipelineIngress>,
    snapshots: LatestSnapshot,
    responses: TerminalResponses,
    line_index: JournalLineIndex,
    outbound: OutboundQueue,
    closed: Arc<AtomicBool>,
    parser_stopped: Arc<AtomicBool>,
    reader_worker: Option<JoinHandle<Result<(), LocalSessionError>>>,
    response_worker: Option<JoinHandle<()>>,
    writer_worker: Option<JoinHandle<Result<(), LocalSessionError>>>,
    finished: bool,
}

/// Cloneable control/view handle for GUI or CLI attachments.
///
/// The weak PTY reference ensures attachments never extend the transport lifetime;
/// immutable snapshots remain readable after the process exits.
#[derive(Clone, Debug)]
pub struct LocalTerminalHandle {
    pty: Weak<Mutex<PtySession>>,
    ingress: PipelineIngress,
    snapshots: LatestSnapshot,
    outbound: OutboundQueue,
    closed: Arc<AtomicBool>,
}

impl LocalTerminalHandle {
    #[must_use]
    pub fn snapshots(&self) -> LatestSnapshot {
        self.snapshots.clone()
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.pty.upgrade().and_then(|pty| {
            pty.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .process_id()
        })
    }

    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn send_input(&self, action: &InputAction) -> Result<(), LocalSessionError> {
        if self.is_closed() {
            return Err(LocalSessionError::Closed);
        }
        let modes = self
            .snapshots
            .latest()
            .map_or_else(TerminalModes::default, |snapshot| snapshot.terminal_modes);
        let bytes = InputEncoder::encode(action, modes)?;
        if bytes.is_empty() {
            return Ok(());
        }
        self.outbound
            .try_submit_user(bytes)
            .map_err(|error| match error {
                OutboundError::Full => LocalSessionError::InputBackpressure,
                OutboundError::Closed => LocalSessionError::Closed,
            })
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), LocalSessionError> {
        if self.is_closed() {
            return Err(LocalSessionError::Closed);
        }
        self.ingress.resize_blocking(size)?;
        self.pty
            .upgrade()
            .ok_or(LocalSessionError::Closed)?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resize(size)?;
        Ok(())
    }
}

impl LocalTerminalSession {
    pub fn spawn(
        profile: &LocalProfile,
        size: TerminalSize,
        journal_path: impl AsRef<Path>,
        ingress_capacity: usize,
    ) -> Result<Self, LocalSessionError> {
        Self::spawn_inner(profile, size, journal_path.as_ref(), ingress_capacity, None)
    }

    #[cfg(unix)]
    pub fn spawn_guarded(
        profile: &LocalProfile,
        size: TerminalSize,
        journal_path: impl AsRef<Path>,
        ingress_capacity: usize,
        guardian_executable: impl AsRef<Path>,
    ) -> Result<Self, LocalSessionError> {
        Self::spawn_inner(
            profile,
            size,
            journal_path.as_ref(),
            ingress_capacity,
            Some(guardian_executable.as_ref()),
        )
    }

    fn spawn_inner(
        profile: &LocalProfile,
        size: TerminalSize,
        journal_path: &Path,
        ingress_capacity: usize,
        #[cfg(unix)] guardian_executable: Option<&Path>,
        #[cfg(windows)] _guardian_executable: Option<&Path>,
    ) -> Result<Self, LocalSessionError> {
        let pipeline = TerminalPipeline::spawn(journal_path, size, ingress_capacity)?;
        let ingress = pipeline.ingress().ok_or(LocalSessionError::Closed)?;
        let snapshots = pipeline.snapshots();
        let responses = pipeline.responses();
        let line_index = pipeline.line_index();

        #[cfg(unix)]
        let mut pty = match guardian_executable {
            Some(guardian) => PtySession::spawn_guarded(profile, size, guardian)?,
            None => PtySession::spawn(profile, size)?,
        };
        #[cfg(windows)]
        let mut pty = PtySession::spawn(profile, size)?;
        let mut reader = pty.take_reader()?;
        let pty = Arc::new(Mutex::new(pty));
        let outbound = OutboundQueue::new();
        let closed = Arc::new(AtomicBool::new(false));
        let parser_stopped = Arc::new(AtomicBool::new(false));

        let writer_pty = Arc::clone(&pty);
        let writer_outbound = outbound.clone();
        let writer_close = outbound.clone();
        let writer_worker = thread::Builder::new()
            .name("cshell-local-pty-writer".to_owned())
            .spawn(move || {
                while let Some(bytes) = writer_outbound.next_blocking() {
                    let result = writer_pty
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .write_all(&bytes);
                    if let Err(error) = result {
                        writer_close.close();
                        return Err(LocalSessionError::Pty(error));
                    }
                }
                Ok(())
            })
            .map_err(LocalPtyError::Io)?;

        let response_source = responses.clone();
        let response_outbound = outbound.clone();
        let response_stopped = Arc::clone(&parser_stopped);
        let response_worker = thread::Builder::new()
            .name("cshell-terminal-responses".to_owned())
            .spawn(move || {
                loop {
                    let batches = response_source.wait_and_drain(RESPONSE_POLL_INTERVAL);
                    let was_empty = batches.is_empty();
                    for bytes in batches {
                        // A closed writer means the child transport is already gone;
                        // keep draining until the parser itself has stopped.
                        let _result = response_outbound.submit_control_blocking(bytes);
                    }
                    if response_stopped.load(Ordering::Acquire) && was_empty {
                        break;
                    }
                }
            })
            .map_err(LocalPtyError::Io)?;

        let reader_ingress = ingress.clone();
        let reader_worker = thread::Builder::new()
            .name("cshell-local-pty-reader".to_owned())
            .spawn(move || {
                let mut buffer = vec![0_u8; OUTPUT_READ_BUFFER_SIZE];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => return Ok(()),
                        Ok(count) => reader_ingress
                            .submit_blocking(buffer[..count].to_vec())
                            .map_err(|error| match error {
                                SubmitError::Backpressure(_) => unreachable!("blocking submit"),
                                SubmitError::Closed(_) => LocalSessionError::Closed,
                            })?,
                        Err(error) if is_transport_eof(&error) => return Ok(()),
                        Err(error) => return Err(LocalSessionError::OutputRead(error)),
                    }
                }
            })
            .map_err(LocalPtyError::Io)?;

        Ok(Self {
            pty: Some(pty),
            pipeline: Some(pipeline),
            ingress: Some(ingress),
            snapshots,
            responses,
            line_index,
            outbound,
            closed,
            parser_stopped,
            reader_worker: Some(reader_worker),
            response_worker: Some(response_worker),
            writer_worker: Some(writer_worker),
            finished: false,
        })
    }

    #[must_use]
    pub fn snapshots(&self) -> LatestSnapshot {
        self.snapshots.clone()
    }

    #[must_use]
    pub fn line_index(&self) -> JournalLineIndex {
        self.line_index.clone()
    }

    #[must_use]
    pub fn handle(&self) -> LocalTerminalHandle {
        LocalTerminalHandle {
            pty: self.pty.as_ref().map_or_else(Weak::new, Arc::downgrade),
            ingress: self
                .ingress
                .as_ref()
                .map_or_else(PipelineIngress::closed, Clone::clone),
            snapshots: self.snapshots.clone(),
            outbound: self.outbound.clone(),
            closed: Arc::clone(&self.closed),
        }
    }

    #[must_use]
    pub fn process_id(&self) -> Option<u32> {
        self.pty.as_ref().and_then(|pty| {
            pty.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .process_id()
        })
    }

    pub fn send_input(&self, action: &InputAction) -> Result<(), LocalSessionError> {
        self.handle().send_input(action)
    }

    pub fn resize(&self, size: TerminalSize) -> Result<(), LocalSessionError> {
        self.handle().resize(size)
    }

    pub fn wait_for_exit(
        &mut self,
        timeout: Duration,
    ) -> Result<LocalSessionExit, LocalSessionError> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(exit) = self.try_wait()? {
                return Ok(exit);
            }
            if Instant::now() >= deadline {
                self.kill_child()?;
                self.finish()?;
                return Err(LocalSessionError::ExitTimeout(timeout));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub fn try_wait(&mut self) -> Result<Option<LocalSessionExit>, LocalSessionError> {
        if self.finished {
            return Ok(None);
        }
        let status = self
            .pty
            .as_ref()
            .ok_or(LocalSessionError::Closed)?
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_wait()?;
        let Some(status) = status else {
            return Ok(None);
        };
        let exit = LocalSessionExit {
            success: status.success(),
            code: status.exit_code(),
            signal: status.signal().map(str::to_owned),
        };
        self.finish()?;
        Ok(Some(exit))
    }

    pub fn kill(&mut self) -> Result<(), LocalSessionError> {
        self.kill_child()?;
        self.finish()
    }

    fn kill_child(&self) -> Result<(), LocalSessionError> {
        if let Some(pty) = &self.pty {
            pty.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .kill()?;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<(), LocalSessionError> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.closed.store(true, Ordering::Release);

        self.outbound.close();
        let writer_result = join_worker(self.writer_worker.take(), "writer");

        if let Some(pty) = &self.pty {
            pty.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .close_input();
        }
        self.pty.take();

        let reader_result = join_worker(self.reader_worker.take(), "reader");
        self.ingress.take();
        let pipeline_result = self.pipeline.take().map(TerminalPipeline::shutdown);

        self.parser_stopped.store(true, Ordering::Release);
        self.responses.wake_waiters();
        let response_result = self
            .response_worker
            .take()
            .map(|worker| {
                worker
                    .join()
                    .map_err(|_| LocalSessionError::WorkerPanicked("response"))
            })
            .transpose();

        writer_result?;
        reader_result?;
        if let Some(result) = pipeline_result {
            result?;
        }
        response_result?;
        Ok(())
    }
}

impl Drop for LocalTerminalSession {
    fn drop(&mut self) {
        if !self.finished {
            let _kill_result = self.kill_child();
            let _finish_result = self.finish();
        }
    }
}

fn join_worker(
    worker: Option<JoinHandle<Result<(), LocalSessionError>>>,
    name: &'static str,
) -> Result<(), LocalSessionError> {
    let Some(worker) = worker else {
        return Ok(());
    };
    worker
        .join()
        .map_err(|_| LocalSessionError::WorkerPanicked(name))?
}

fn is_transport_eof(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::UnexpectedEof
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{LocalTerminalSession, OutboundQueue};
    use cshell_domain::TerminalSize;
    use cshell_local::{LocalProfile, WorkingDirectoryPolicy};
    use cshell_output_store::scan;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn terminal_control_responses_have_priority_over_queued_user_input() {
        let queue = OutboundQueue::new();
        queue.try_submit_user(b"user".to_vec()).unwrap();
        queue.submit_control_blocking(b"control".to_vec()).unwrap();

        assert_eq!(queue.next_blocking().unwrap(), b"control");
        assert_eq!(queue.next_blocking().unwrap(), b"user");
        queue.close();
        assert!(queue.next_blocking().is_none());
    }

    #[test]
    fn daemon_hosts_real_pty_through_journal_and_terminal_pipeline() {
        #[cfg(windows)]
        let profile = LocalProfile {
            name: "daemon PTY probe".to_owned(),
            program: PathBuf::from("whoami.exe"),
            args: Vec::new(),
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        let profile = LocalProfile {
            name: "daemon PTY probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-lc".to_owned(), "printf CSHELL_DAEMON_PTY_OK".to_owned()],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        let directory = tempfile::tempdir().unwrap();
        let journal_path = directory.path().join("local-session.csjr");
        let mut session =
            LocalTerminalSession::spawn(&profile, TerminalSize::cells(24, 80), &journal_path, 32)
                .unwrap();
        assert!(session.process_id().is_some());
        let snapshots = session.snapshots();

        let status = session.wait_for_exit(Duration::from_secs(5)).unwrap();
        assert!(status.success, "PTY exit: {status:?}");

        let snapshot = snapshots.latest().unwrap();
        assert!(snapshot.generation > 0);
        let rendered: String = snapshot.cells.iter().map(|cell| cell.character).collect();
        assert!(!rendered.trim().is_empty());

        let report = scan(journal_path, false).unwrap();
        assert!(!report.records.is_empty());
        assert!(
            report
                .records
                .iter()
                .any(|record| !record.payload.is_empty())
        );
    }

    #[test]
    fn killing_a_running_session_releases_pty_workers_promptly() {
        #[cfg(windows)]
        let profile = LocalProfile {
            name: "daemon PTY kill probe".to_owned(),
            program: PathBuf::from("cmd.exe"),
            args: vec![
                "/D".to_owned(),
                "/S".to_owned(),
                "/C".to_owned(),
                "ping -n 30 127.0.0.1 >nul".to_owned(),
            ],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        #[cfg(not(windows))]
        let profile = LocalProfile {
            name: "daemon PTY kill probe".to_owned(),
            program: PathBuf::from("/bin/sh"),
            args: vec!["-lc".to_owned(), "sleep 30".to_owned()],
            cwd_policy: WorkingDirectoryPolicy::Inherit,
            env_overrides: BTreeMap::new(),
        };

        let directory = tempfile::tempdir().unwrap();
        let mut session = LocalTerminalSession::spawn(
            &profile,
            TerminalSize::cells(24, 80),
            directory.path().join("killed-session.csjr"),
            32,
        )
        .unwrap();
        let started = std::time::Instant::now();
        session.kill().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "PTY worker shutdown took {:?}",
            started.elapsed()
        );
    }
}
