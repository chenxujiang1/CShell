use cshell_domain::TerminalSize;
use cshell_terminal::{AlacrittyTerminalEngine, FrameSnapshot, TerminalEngine};
use std::sync::{Arc, Mutex, mpsc};
enum Work {
    Reset(u64),
    Feed {
        epoch: u64,
        data: Vec<u8>,
        rows: Option<u16>,
        cols: Option<u16>,
    },
}
#[derive(Clone, Default)]
pub(super) struct View {
    pub epoch: u64,
    pub snapshot: Option<Arc<FrameSnapshot>>,
    pub generation: u64,
}
pub(super) struct ReplayWorker {
    sender: Option<mpsc::SyncSender<Work>>,
    view: Arc<Mutex<View>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl ReplayWorker {
    pub fn start() -> Result<Self, std::io::Error> {
        let (sender, receiver) = mpsc::sync_channel::<Work>(2);
        let view = Arc::new(Mutex::new(View::default()));
        let shared = view.clone();
        let worker = std::thread::Builder::new()
            .name("cshell-read-only-replay".into())
            .spawn(move || {
                let mut engine = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
                let mut epoch = 0;
                let mut size = (24, 80);
                while let Ok(work) = receiver.recv() {
                    match work {
                        Work::Reset(next) if next >= epoch => {
                            epoch = next;
                            size = (24, 80);
                            engine = AlacrittyTerminalEngine::new(TerminalSize::cells(24, 80));
                        }
                        Work::Reset(_) => continue,
                        Work::Feed {
                            epoch: next,
                            data,
                            rows,
                            cols,
                        } => {
                            if next != epoch || data.len() > 64 * 1024 {
                                continue;
                            }
                            if let (Some(rows), Some(cols)) = (rows, cols) {
                                let next = (rows.clamp(1, 200), cols.clamp(1, 512));
                                if next != size {
                                    size = next;
                                    engine.resize(TerminalSize::cells(size.0, size.1));
                                }
                            }
                            // Drop every response. No transport, clipboard observer,
                            // trigger matcher, journal writer or vault exists here.
                            let _ = engine.feed(&data);
                        }
                    }
                    let mut view = shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    view.epoch = epoch;
                    view.snapshot = Some(Arc::new(engine.snapshot()));
                    view.generation = view.generation.wrapping_add(1);
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            view,
            worker: Some(worker),
        })
    }
    pub fn reset(&self, epoch: u64) -> bool {
        self.sender
            .as_ref()
            .is_some_and(|s| s.try_send(Work::Reset(epoch)).is_ok())
    }
    pub fn feed(&self, epoch: u64, data: Vec<u8>, rows: Option<u16>, cols: Option<u16>) -> bool {
        self.sender.as_ref().is_some_and(|s| {
            s.try_send(Work::Feed {
                epoch,
                data,
                rows,
                cols,
            })
            .is_ok()
        })
    }
    pub fn view(&self) -> View {
        self.view
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}
impl Drop for ReplayWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
