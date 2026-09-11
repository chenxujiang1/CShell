use cshell_render::{
    TerminalInteractionError, TerminalSearchOptions, TerminalSearchResult, search_terminal_snapshot,
};
use cshell_terminal::FrameSnapshot;
use std::sync::{Arc, Condvar, Mutex, PoisonError};

#[derive(Clone, Debug)]
pub struct TerminalSearchRequest {
    pub revision: u64,
    pub snapshot: Arc<FrameSnapshot>,
    pub query: String,
    pub options: TerminalSearchOptions,
}

#[derive(Debug)]
pub struct TerminalSearchResponse {
    pub revision: u64,
    pub snapshot_generation: u64,
    pub result: Result<TerminalSearchResult, TerminalInteractionError>,
}

#[derive(Default)]
struct SearchState {
    pending: Option<TerminalSearchRequest>,
    completed: Option<TerminalSearchResponse>,
    stopped: bool,
}

pub struct TerminalSearchWorker {
    state: Arc<(Mutex<SearchState>, Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl TerminalSearchWorker {
    pub fn start() -> Result<Self, std::io::Error> {
        let state = Arc::new((Mutex::new(SearchState::default()), Condvar::new()));
        let worker_state = Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name("cshell-terminal-search".to_owned())
            .spawn(move || {
                loop {
                    let request = {
                        let (lock, ready) = worker_state.as_ref();
                        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        while state.pending.is_none() && !state.stopped {
                            state = ready.wait(state).unwrap_or_else(PoisonError::into_inner);
                        }
                        if state.stopped {
                            break;
                        }
                        state.pending.take()
                    };
                    let Some(request) = request else {
                        continue;
                    };
                    let response = TerminalSearchResponse {
                        revision: request.revision,
                        snapshot_generation: request.snapshot.generation,
                        result: search_terminal_snapshot(
                            &request.snapshot,
                            &request.query,
                            request.options,
                        ),
                    };
                    let (lock, _) = worker_state.as_ref();
                    let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    if state.stopped {
                        break;
                    }
                    state.completed = Some(response);
                }
            })?;
        Ok(Self {
            state,
            worker: Some(worker),
        })
    }

    pub fn submit(&self, request: TerminalSearchRequest) {
        let (lock, ready) = self.state.as_ref();
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.stopped {
            state.pending = Some(request);
            ready.notify_one();
        }
    }

    pub fn take_latest(&self) -> Option<TerminalSearchResponse> {
        let (lock, _) = self.state.as_ref();
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .completed
            .take()
    }
}

impl Drop for TerminalSearchWorker {
    fn drop(&mut self) {
        let (lock, ready) = self.state.as_ref();
        {
            let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
            state.stopped = true;
            state.pending = None;
            state.completed = None;
            ready.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            let _result = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TerminalSearchRequest, TerminalSearchWorker};
    use cshell_render::TerminalSearchOptions;
    use cshell_terminal::{Cell, CellWidth, FrameSnapshot, TerminalModes};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[test]
    fn worker_searches_a_snapshot_without_blocking_the_caller() {
        let worker = TerminalSearchWorker::start()
            .unwrap_or_else(|error| panic!("search worker must start: {error}"));
        worker.submit(TerminalSearchRequest {
            revision: 7,
            snapshot: Arc::new(FrameSnapshot {
                generation: 11,
                rows: 1,
                cols: 4,
                cursor_row: 0,
                cursor_col: 0,
                cursor_appearance: Default::default(),
                terminal_modes: TerminalModes::default(),
                cells: "Rust"
                    .chars()
                    .map(|character| Cell::new(character, CellWidth::Single, Default::default()))
                    .collect(),
            }),
            query: "rust".to_owned(),
            options: TerminalSearchOptions::default(),
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match worker.take_latest() {
                Some(response) => {
                    assert_eq!(response.revision, 7);
                    assert_eq!(response.snapshot_generation, 11);
                    assert_eq!(
                        response
                            .result
                            .unwrap_or_else(|error| panic!("search must succeed: {error}"))
                            .matches
                            .len(),
                        1
                    );
                    break;
                }
                None if Instant::now() < deadline => {
                    std::thread::yield_now();
                }
                other => panic!("search worker did not respond: {other:?}"),
            }
        }
    }
}
