//! The call history: every finished call (followed or not), the radios in it, radio events and
//! recordings, in SQLite (schema v2, `schema`), on the SD card or in RAM without one.
//!
//! One writer thread owns the writes. It is fed by a channel (calls as they close, the vocoder's
//! counts after, radio events, sites, recordings), commits every 10 s or on a flush (fewer, larger
//! SD writes), and prunes to the retention once an hour, whole hours at a time. The API reads
//! through the store's read-only connection.

pub mod schema;
pub mod store;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::services::config::radio::History as HistoryConfig;
use crate::util::time::unix_ms;
use store::{CallRow, RecordingRow, SiteInfo, Store, UnitEventKind, UnitNote, VoiceResult};

pub const FILE: &str = "scanner-history.sqlite";
const RAM_PATH: &str = "/tmp/scanner-history.sqlite";
/// Most space in RAM, with no card.
const MAX_BYTES_RAM: u64 = 16 << 20;
const COMMIT_EVERY: Duration = Duration::from_secs(10);
const PRUNE_EVERY: Duration = Duration::from_secs(3_600);
/// Distinct radio events kept between commits (the rest are dropped).
const MAX_NOTES: usize = 20_000;
const DAY_MS: u64 = 86_400_000;

enum Input {
    Call(CallRow),
    Voice(VoiceResult),
    Unit { site: String, note: UnitNote },
    Site(SiteInfo),
    Recordings(Vec<RecordingRow>),
    RecordingsGone(Vec<String>),
    Limits(Limits),
    Flush(Sender<()>),
}

/// Where the history's writers send. The default reaches no history (tests).
#[derive(Clone, Default, Debug)]
pub struct HistoryTx(Option<Sender<Input>>);

impl HistoryTx {
    pub fn call(&self, row: CallRow) {
        self.send(Input::Call(row));
    }

    pub fn voice(&self, v: VoiceResult) {
        self.send(Input::Voice(v));
    }

    pub fn unit(&self, site: &str, unit: u32, tg: u32, kind: UnitEventKind, at_ms: u64) {
        let note = UnitNote { unit, tg, kind, first_ms: at_ms, last_ms: at_ms, count: 1 };
        self.send(Input::Unit { site: site.to_string(), note });
    }

    pub fn site(&self, s: SiteInfo) {
        self.send(Input::Site(s));
    }

    pub fn recordings(&self, rows: Vec<RecordingRow>) {
        self.send(Input::Recordings(rows));
    }

    pub fn recordings_gone(&self, files: Vec<String>) {
        if !files.is_empty() {
            self.send(Input::RecordingsGone(files));
        }
    }

    fn send(&self, input: Input) -> bool {
        self.0.as_ref().is_some_and(|tx| tx.send(input).is_ok())
    }
}

/// How much is kept.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub retention_days: u32,
    pub max_bytes: u64,
}

pub struct History {
    store: Arc<Store>,
    tx: HistoryTx,
    pub on_sd: bool,
    limits: std::sync::Mutex<Limits>,
}

impl History {
    /// Open the history on the card (`sd`: its mounted directory), or in RAM without a card;
    /// start the writer.
    pub fn open(sd: Option<&Path>, cfg: &HistoryConfig, sites: &[SiteInfo]) -> anyhow::Result<Arc<History>> {
        let (path, limits) = match sd {
            Some(dir) => (dir.join(FILE), Limits { retention_days: cfg.retention_days, max_bytes: cfg.sd_max_mb.saturating_mul(1 << 20) }),
            None => (PathBuf::from(RAM_PATH), Limits { retention_days: cfg.retention_days, max_bytes: MAX_BYTES_RAM }),
        };
        let store = Arc::new(Store::open(&path).with_context(|| format!("opening {}", path.display()))?);
        for s in sites {
            store.note_site(s)?;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let writer = store.clone();
        std::thread::Builder::new().name("history".into()).spawn(move || write_loop(&writer, rx, limits))?;
        Ok(Arc::new(History { store, tx: HistoryTx(Some(tx)), on_sd: sd.is_some(), limits: std::sync::Mutex::new(limits) }))
    }

    pub fn sender(&self) -> HistoryTx {
        self.tx.clone()
    }

    pub fn limits(&self) -> Limits {
        *self.limits.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// New limits from the radio settings: the retention applies at once (RAM keeps its cap).
    pub fn set_limits(&self, cfg: &HistoryConfig) {
        let limits = Limits {
            retention_days: cfg.retention_days,
            max_bytes: if self.on_sd { cfg.sd_max_mb.saturating_mul(1 << 20) } else { MAX_BYTES_RAM },
        };
        *self.limits.lock().unwrap_or_else(|e| e.into_inner()) = limits;
        self.tx.send(Input::Limits(limits));
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Run a query off the async runtime.
    pub async fn query<T: Send + 'static>(&self, f: impl FnOnce(&Store) -> rusqlite::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let store = self.store.clone();
        Ok(tokio::task::spawn_blocking(move || f(&store)).await??)
    }

    /// Commit everything sent so far; waits up to `max`.
    pub async fn flush(&self, max: Duration) {
        let (done, wait) = std::sync::mpsc::channel();
        if self.tx.send(Input::Flush(done)) {
            let _ = tokio::task::spawn_blocking(move || wait.recv_timeout(max)).await;
        }
    }
}

type NoteKey = (String, u32, u32, UnitEventKind);

struct Pending {
    calls: Vec<CallRow>,
    voice: Vec<VoiceResult>,
    notes: HashMap<NoteKey, UnitNote>,
}

impl Pending {
    fn commit(&mut self, store: &Store) {
        if !self.calls.is_empty() {
            if let Err(e) = store.insert_calls(&self.calls) {
                tracing::error!("history: {} calls not stored: {e}", self.calls.len());
            }
            self.calls.clear();
        }
        if !self.voice.is_empty() {
            if let Err(e) = store.voice_results(&self.voice) {
                tracing::warn!("history: voice counts not stored: {e}");
            }
            self.voice.clear();
        }
        let mut by_site: HashMap<String, Vec<UnitNote>> = HashMap::new();
        for ((site, ..), n) in self.notes.drain() {
            by_site.entry(site).or_default().push(n);
        }
        for (site, notes) in by_site {
            if let Err(e) = store.note_units(&site, &notes) {
                tracing::warn!("history: radio events not stored: {e}");
            }
        }
    }
}

fn write_loop(store: &Store, rx: Receiver<Input>, mut limits: Limits) {
    let mut p = Pending { calls: Vec::new(), voice: Vec::new(), notes: HashMap::new() };
    let mut last_commit = Instant::now();
    let mut last_prune: Option<Instant> = None;
    loop {
        match rx.recv_timeout(COMMIT_EVERY.saturating_sub(last_commit.elapsed())) {
            Ok(Input::Call(r)) => p.calls.push(r),
            Ok(Input::Voice(v)) => {
                let same = |c: &&mut CallRow| c.site == v.site && c.call_id == v.call_id && c.started_ms == v.started_ms;
                match p.calls.iter_mut().find(same) {
                    Some(c) => {
                        c.frames = v.frames;
                        c.frame_errors = v.frame_errors;
                    }
                    None => p.voice.push(v),
                }
            }
            Ok(Input::Unit { site, note }) => {
                let key = (site, note.unit, note.tg, note.kind);
                if let Some(n) = p.notes.get_mut(&key) {
                    n.count += 1;
                    n.last_ms = n.last_ms.max(note.last_ms);
                } else if p.notes.len() < MAX_NOTES {
                    p.notes.insert(key, note);
                }
            }
            Ok(Input::Site(s)) => {
                if let Err(e) = store.note_site(&s) {
                    tracing::warn!("history: site {} not stored: {e}", s.id);
                }
            }
            Ok(Input::Recordings(rows)) => {
                // Their calls first, so the recordings link to them.
                p.commit(store);
                last_commit = Instant::now();
                if let Err(e) = store.add_recordings(&rows) {
                    tracing::warn!("history: recordings not listed: {e}");
                }
            }
            Ok(Input::RecordingsGone(files)) => {
                if let Err(e) = store.remove_recordings(&files) {
                    tracing::warn!("history: recordings not removed: {e}");
                }
            }
            Ok(Input::Flush(done)) => {
                p.commit(store);
                last_commit = Instant::now();
                let _ = done.send(());
            }
            Ok(Input::Limits(l)) => {
                limits = l;
                last_prune = None;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                p.commit(store);
                return;
            }
        }
        if last_commit.elapsed() >= COMMIT_EVERY {
            p.commit(store);
            last_commit = Instant::now();
        }
        if last_prune.is_none_or(|t| t.elapsed() >= PRUNE_EVERY) {
            last_prune = Some(Instant::now());
            prune(store, limits);
        }
    }
}

fn prune(store: &Store, limits: Limits) {
    let before = unix_ms().saturating_sub(u64::from(limits.retention_days) * DAY_MS);
    match store.prune(before) {
        Ok(n) if n > 0 => tracing::info!("history: {n} calls older than {} days removed", limits.retention_days),
        Ok(_) => {}
        Err(e) => tracing::warn!("history: prune failed: {e}"),
    }
    match store.trim_to(limits.max_bytes) {
        Ok(n) if n > 0 => tracing::info!("history: {n} oldest calls removed to stay under {} MB", limits.max_bytes >> 20),
        Ok(_) => {}
        Err(e) => tracing::warn!("history: trim failed: {e}"),
    }
}

#[cfg(test)]
mod tests;
