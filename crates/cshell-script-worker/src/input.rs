//! One stdin reader owns frame decoding. Heartbeats never wait behind a JS
//! callback, and watchdog exit never depends on the interpreter interrupt hook.
use cshell_script_runtime::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

pub(super) struct WorkerInput {
    replies: Receiver<HostReply>,
}
impl WorkerInput {
    pub(super) fn start() -> Result<(WorkerStart, Self), ScriptWorkerError> {
        let started = Instant::now();
        let last_heartbeat = Arc::new(AtomicU64::new(0));
        let watchdog = last_heartbeat.clone();
        std::thread::Builder::new()
            .name("script-parent-watchdog".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let now = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                    if now.saturating_sub(watchdog.load(Ordering::Acquire)) >= WORKER_LIVENESS_MS {
                        // No source, parameter, exception or process identifier is logged.
                        std::process::exit(3);
                    }
                }
            })
            .map_err(|_| ScriptWorkerError::Unavailable)?;
        let (starts, start_receiver) = mpsc::sync_channel(1);
        let (replies, receiver) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("script-private-input".into())
            .spawn(move || {
                let mut stdin = std::io::stdin();
                let start = match read_frame::<HostFrame>(&mut stdin) {
                    Ok(HostFrame::Start { start }) => start,
                    _ => std::process::exit(3),
                };
                let run_id = start.run_id;
                last_heartbeat.store(started.elapsed().as_millis() as u64, Ordering::Release);
                if starts.send(start).is_err() {
                    std::process::exit(3);
                }
                loop {
                    match read_frame::<HostFrame>(&mut stdin) {
                        Ok(HostFrame::Heartbeat { run_id: received }) if received == run_id => {
                            last_heartbeat.store(
                                started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                                Ordering::Release,
                            );
                        }
                        Ok(HostFrame::Reply { reply }) if reply.run_id == run_id => {
                            // An unexpected extra reply must never queue unbounded
                            // values or block heartbeats behind a full channel.
                            if replies.try_send(reply).is_err() {
                                std::process::exit(3);
                            }
                        }
                        // EOF, partial/invalid frame, repeated start and wrong IDs
                        // terminate even while JS is looping or blocked in an RPC.
                        _ => std::process::exit(3),
                    }
                }
            })
            .map_err(|_| ScriptWorkerError::Unavailable)?;
        let start = start_receiver
            .recv()
            .map_err(|_| ScriptWorkerError::Protocol)?;
        Ok((start, Self { replies: receiver }))
    }
    pub(super) fn reply(&self) -> Result<HostReply, ScriptWorkerError> {
        self.replies.recv().map_err(|_| ScriptWorkerError::Protocol)
    }
}
