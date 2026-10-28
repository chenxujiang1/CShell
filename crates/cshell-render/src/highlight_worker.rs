use crate::{LogDecorations, LogHighlight, LogPage, TerminalDecorations, TerminalHighlight};
use cshell_domain::{HighlightRule, SessionId};
use cshell_highlight::{CompiledHighlights, HighlightSpan};
use cshell_terminal::{CellWidth, FrameSnapshot};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
enum Source {
    Terminal(Arc<FrameSnapshot>),
    Log(Arc<LogPage>),
}
fn rule_identity(revision: u64, rules: &[HighlightRule]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    revision.hash(&mut hash);
    rules.hash(&mut hash);
    hash.finish()
}
struct Work {
    session: SessionId,
    revision: u64,
    rules: Vec<HighlightRule>,
    source: Source,
}
#[derive(Clone, Default)]
struct ResultSet {
    stamp: (u64, u64, u64, u64),
    serial: u64,
    terminal: Arc<[TerminalHighlight]>,
    log: Arc<[LogHighlight]>,
    complete: bool,
}
/// One bounded worker; matching never runs in terminal parsing or the UI thread.
pub struct HighlightWorker {
    sender: Option<mpsc::SyncSender<Work>>,
    results: Arc<Mutex<BTreeMap<(SessionId, bool), ResultSet>>>,
    requested: BTreeMap<(SessionId, bool), ((u64, u64, u64, u64), u64)>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl std::fmt::Debug for HighlightWorker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HighlightWorker").finish_non_exhaustive()
    }
}
impl HighlightWorker {
    pub fn start() -> Result<Self, std::io::Error> {
        let (sender, receiver) = mpsc::sync_channel::<Work>(4);
        let results = Arc::new(Mutex::new(BTreeMap::new()));
        let shared = results.clone();
        let worker = std::thread::Builder::new()
            .name("cshell-highlight".into())
            .spawn(move || {
                let mut compiled: BTreeMap<
                    SessionId,
                    (u64, Vec<HighlightRule>, CompiledHighlights),
                > = BTreeMap::new();
                let mut cache: BTreeMap<(SessionId, bool, u64), (String, Vec<HighlightSpan>, u64)> =
                    BTreeMap::new();
                let mut order = VecDeque::new();
                let mut serial = 0u64;
                while let Ok(work) = receiver.recv() {
                    if compiled
                        .get(&work.session)
                        .is_none_or(|(rev, rules, _)| *rev != work.revision || *rules != work.rules)
                    {
                        let Ok(rules) = CompiledHighlights::new(&work.rules) else {
                            continue;
                        };
                        if compiled.len() >= 16 {
                            compiled.clear();
                            cache.clear();
                            order.clear();
                        }
                        compiled.insert(work.session, (work.revision, work.rules, rules));
                        cache.clear();
                        order.clear();
                    }
                    let Some((_, _, rules)) = compiled.get(&work.session) else {
                        continue;
                    };
                    let started = Instant::now();
                    let mut complete = true;
                    let mut terminal = Vec::new();
                    let mut log = Vec::new();
                    let (kind, stamp, rows) = match &work.source {
                        Source::Terminal(snapshot) => (
                            false,
                            (snapshot.generation, work.revision, 0, 0),
                            usize::from(snapshot.rows),
                        ),
                        Source::Log(page) => (
                            true,
                            (
                                page.revision,
                                work.revision,
                                page.anchor_line_id,
                                page.source_id.0,
                            ),
                            page.rows.len(),
                        ),
                    };
                    for i in 0..rows {
                        let (line_id, text, mapping) = match &work.source {
                            Source::Terminal(snapshot) => {
                                let Some(cells) = snapshot.row(i as u16) else {
                                    continue;
                                };
                                let mut text = String::new();
                                let mut mapping = Vec::new();
                                for (column, cell) in cells.iter().enumerate() {
                                    if matches!(
                                        cell.width,
                                        CellWidth::WideSpacer | CellWidth::LeadingWideSpacer
                                    ) {
                                        continue;
                                    }
                                    let start = text.len();
                                    text.push(cell.character);
                                    for c in cell.zerowidth() {
                                        text.push(*c);
                                    }
                                    mapping.push((
                                        start,
                                        text.len(),
                                        column as u16,
                                        if cell.width == CellWidth::Wide { 2 } else { 1 },
                                    ));
                                }
                                (i as u64, text, Some(mapping))
                            }
                            Source::Log(page) => {
                                (page.rows[i].line_id, page.rows[i].text.to_string(), None)
                            }
                        };
                        let key = (work.session, kind, line_id);
                        let spans = if let Some((old, spans, rev)) = cache
                            .get(&key)
                            .filter(|(old, _, rev)| *old == text && *rev == work.revision)
                        {
                            let _ = (old, rev);
                            spans.clone()
                        } else {
                            if text.len() <= cshell_highlight::MAX_LINE_BYTES
                                && started.elapsed() > Duration::from_millis(4)
                            {
                                complete = false;
                                continue;
                            }
                            let spans = rules.spans(&text);
                            if !cache.contains_key(&key) {
                                order.push_back(key);
                            }
                            cache.insert(key, (text, spans.clone(), work.revision));
                            while cache.len() > 512 {
                                if let Some(old) = order.pop_front() {
                                    cache.remove(&old);
                                } else {
                                    break;
                                }
                            }
                            spans
                        };
                        for span in spans {
                            if let Some(mapping) = &mapping {
                                let start = mapping
                                    .iter()
                                    .find(|(_, end, _, _)| *end > span.range.start as usize)
                                    .map(|(_, _, col, _)| *col);
                                let end = mapping
                                    .iter()
                                    .rev()
                                    .find(|(start, _, _, _)| *start < span.range.end as usize)
                                    .map(|(_, _, col, width)| col.saturating_add(*width));
                                if let (Some(start), Some(end)) = (start, end) {
                                    terminal.push(TerminalHighlight {
                                        row: i as u16,
                                        start,
                                        end,
                                        style: span.style,
                                    });
                                }
                            } else {
                                log.push(LogHighlight {
                                    line_id,
                                    range: span.range,
                                    style: span.style,
                                });
                            }
                        }
                        if terminal.len() + log.len() > 16 * 1024 {
                            complete = true;
                            break;
                        }
                    }
                    let mut results = shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if results.len() >= 32 && !results.contains_key(&(work.session, kind)) {
                        results.clear();
                    }
                    serial = serial.wrapping_add(1);
                    results.insert(
                        (work.session, kind),
                        ResultSet {
                            stamp,
                            serial,
                            terminal: terminal.into(),
                            log: log.into(),
                            complete,
                        },
                    );
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            results,
            requested: BTreeMap::new(),
            worker: Some(worker),
        })
    }
    pub fn terminal(
        &mut self,
        session: SessionId,
        revision: u64,
        rules: Vec<HighlightRule>,
        snapshot: Arc<FrameSnapshot>,
        decorations: &mut TerminalDecorations,
    ) {
        let revision = rule_identity(revision, &rules);
        let key = (session, false);
        let stamp = (snapshot.generation, revision, 0, 0);
        let result = self
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        if let Some(result) = &result
            && result.stamp == stamp
            && decorations.highlights != result.terminal
        {
            decorations.highlights = result.terminal.clone();
            decorations.revision = decorations.revision.wrapping_add(1);
        } else if result.as_ref().is_none_or(|r| r.stamp != stamp)
            && !decorations.highlights.is_empty()
        {
            decorations.highlights = Arc::from([]);
            decorations.revision = decorations.revision.wrapping_add(1);
        }
        self.submit(
            key,
            stamp,
            rules,
            Source::Terminal(snapshot),
            result
                .as_ref()
                .is_some_and(|r| r.complete && r.stamp == stamp),
            result.map_or(0, |r| r.serial),
        );
    }
    pub fn generation(&self) -> u64 {
        self.results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(|r| r.serial)
            .max()
            .unwrap_or(0)
    }
    pub fn log(
        &mut self,
        session: SessionId,
        revision: u64,
        rules: Vec<HighlightRule>,
        frame: &crate::LogSurfaceFrame,
        decorations: &mut LogDecorations,
    ) {
        let revision = rule_identity(revision, &rules);
        let visible = frame
            .visual_rows
            .get(frame.visible_rows.clone())
            .unwrap_or(&[])
            .iter()
            .map(|r| r.logical_row_index as usize)
            .collect::<std::collections::BTreeSet<_>>();
        let rows = visible
            .into_iter()
            .take(512)
            .filter_map(|i| frame.page.rows.get(i).cloned())
            .collect::<Vec<_>>();
        let page = Arc::new(LogPage { first_line_id: 1,
            anchor_line_id: rows
                .first()
                .map_or(frame.page.anchor_line_id, |r| r.line_id),
            rows: rows.into(),
            ..frame.page.as_ref().clone()
        });
        let key = (session, true);
        let stamp = (
            page.revision,
            revision,
            page.anchor_line_id,
            page.source_id.0,
        );
        let result = self
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        if let Some(result) = &result
            && result.stamp == stamp
            && decorations.highlights != result.log
        {
            decorations.highlights = result.log.clone();
            decorations.revision = decorations.revision.wrapping_add(1);
        } else if result.as_ref().is_none_or(|r| r.stamp != stamp)
            && !decorations.highlights.is_empty()
        {
            decorations.highlights = Arc::from([]);
            decorations.revision = decorations.revision.wrapping_add(1);
        }
        self.submit(
            key,
            stamp,
            rules,
            Source::Log(page),
            result
                .as_ref()
                .is_some_and(|r| r.complete && r.stamp == stamp),
            result.map_or(0, |r| r.serial),
        );
    }
    fn submit(
        &mut self,
        key: (SessionId, bool),
        stamp: (u64, u64, u64, u64),
        rules: Vec<HighlightRule>,
        source: Source,
        complete: bool,
        serial: u64,
    ) {
        if complete {
            return;
        }
        if self.requested.get(&key) == Some(&(stamp, serial)) {
            return;
        }
        if self.requested.len() > 32 {
            self.requested.clear();
        }
        if self.sender.as_ref().is_some_and(|s| {
            s.try_send(Work {
                session: key.0,
                revision: stamp.1,
                rules,
                source,
            })
            .is_ok()
        }) {
            self.requested.insert(key, (stamp, serial));
        }
    }
}
impl Drop for HighlightWorker {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
