//! Call recordings: one WAV per followed call, driven by the call book's opens and closes.
//!
//! A followed call's start opens a recording; its 20 ms chunks (the live audio broadcast) are
//! appended by call id; its close starts a 2 s drain for the audio still in the pacer, then the
//! WAV is saved (`storage`) and listed. A call with no clear audio leaves no file. The list is the
//! card's recordings found at boot plus this run's, oldest first. Every followed call's voice
//! frames, vocoder errors and silent frames are counted, recorded or not, and go to the history
//! with the recordings (`reconcile` links the card's files to their calls at boot).

pub mod index;
pub mod storage;
pub mod wav;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Serialize, Serializer};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::audio::live::{Audio, AudioChunk};
use crate::hardware::p25core::Lane;
use crate::services::config::radio::{self as radio_config, Storage as StorageKind};
use crate::services::history::store::{RecordingRow, Store as HistoryStore, VoiceResult};
use crate::services::history::HistoryTx;
use crate::services::notices::{Notice, Notices};
use storage::{Retention, SdStatus, Storage, StorageConfig};

/// After a call closes, its audio still in the decoder and pacer comes in for this long.
const DRAIN: Duration = Duration::from_secs(2);
/// A recording with no close and no audio for this long is saved anyway (the close was lost).
const STALE: Duration = Duration::from_secs(300);
const TICK: Duration = Duration::from_millis(100);

pub type Ring = Arc<Mutex<VecDeque<Recording>>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Store {
    #[default]
    Ram,
    Sd,
}

impl From<StorageKind> for Store {
    fn from(k: StorageKind) -> Store {
        match k {
            StorageKind::Ram => Store::Ram,
            StorageKind::Sd => Store::Sd,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct VoiceCounts {
    pub frames: u64,
    pub errors: u64,
    pub silent: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Recording {
    /// The call's id.
    pub id: u64,
    pub site: String,
    pub tg: u32,
    pub source: Option<u32>,
    pub started_unix_ms: u64,
    pub duration_ms: u64,
    pub bytes: u64,
    pub file: String,
    pub store: Store,
    /// Not on the card yet: served from RAM meanwhile.
    #[serde(rename = "sd_pending", serialize_with = "is_some")]
    pub pending: Option<Arc<Vec<u8>>>,
    pub sources: Vec<u32>,
    /// The rest is known for this run's recordings only (a file's name says no more).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lane: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freq_hz: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<VoiceCounts>,
    #[serde(skip)]
    pub path: PathBuf,
}

fn is_some<S: Serializer>(v: &Option<Arc<Vec<u8>>>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_bool(v.is_some())
}

/// A followed call opened.
#[derive(Debug, Clone)]
pub struct CallStart {
    pub call: u64,
    pub site: String,
    pub tg: u32,
    pub source: Option<u32>,
    pub lane: Lane,
    pub freq_hz: Option<u64>,
    pub channel: Option<String>,
    pub started_unix_ms: u64,
    /// Its alias says record it.
    pub record: bool,
}

/// A followed call closed.
#[derive(Debug, Clone)]
pub struct CallEnd {
    pub call: u64,
    pub source: Option<u32>,
    pub sources: Vec<u32>,
}

enum Input {
    Start(CallStart),
    End(CallEnd),
    /// Save every open recording now (shutdown).
    Flush(oneshot::Sender<()>),
}

/// Where the trunking task reports followed calls. The default reaches no recorder.
#[derive(Clone, Default)]
pub struct RecorderTx(Option<mpsc::UnboundedSender<Input>>);

impl RecorderTx {
    pub fn start(&self, s: CallStart) {
        self.send(Input::Start(s));
    }

    pub fn end(&self, e: CallEnd) {
        self.send(Input::End(e));
    }

    fn send(&self, input: Input) -> bool {
        self.0.as_ref().is_some_and(|tx| tx.send(input).is_ok())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub enabled: bool,
    /// Every followed call; off: only those whose alias says record.
    pub every_call: bool,
    pub store: Store,
    pub retention: Retention,
}

impl From<&radio_config::Recording> for Policy {
    fn from(r: &radio_config::Recording) -> Policy {
        Policy {
            enabled: r.enabled,
            every_call: r.every_call,
            store: r.storage.into(),
            retention: Retention {
                ram_max_count: r.ram_max_count as usize,
                sd_max_count: r.sd_max_count as usize,
                sd_max_bytes: r.sd_max_mb.saturating_mul(1024 * 1024),
            },
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    saved: AtomicU64,
    /// Followed calls with no clear audio.
    no_audio: AtomicU64,
    /// Followed calls not recorded because recording is off.
    off: AtomicU64,
    failed: AtomicU64,
    /// Chunks of no open recording, and chunks the recorder fell too far behind to see.
    unmatched_chunks: AtomicU64,
    missed_chunks: AtomicU64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CounterView {
    pub saved: u64,
    pub no_audio: u64,
    pub off: u64,
    pub failed: u64,
    pub unmatched_chunks: u64,
    pub missed_chunks: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoreUsage {
    pub count: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub enabled: bool,
    /// Where new recordings go.
    pub store: Store,
    pub ram: StoreUsage,
    pub ram_max_count: usize,
    pub sd: StoreUsage,
    pub sd_max_count: usize,
    pub sd_max_bytes: u64,
    pub sd_status: SdStatus,
    pub recording_now: usize,
    pub counters: CounterView,
}

struct Shared {
    ring: Ring,
    storage: Arc<Storage>,
    history: HistoryTx,
    notices: Notices,
    policy: Mutex<Policy>,
    counters: Counters,
    recording_now: AtomicU64,
}

impl Shared {
    fn policy(&self) -> Policy {
        *lock(&self.policy)
    }
}

pub struct Recordings {
    shared: Arc<Shared>,
    tx: RecorderTx,
    next_call: u64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Recordings {
    /// Start the card writer and the recorder. `indexed` is the card's listing (`storage::index`).
    pub fn start(cfg: StorageConfig, policy: Policy, indexed: (Vec<Recording>, String), audio: &Audio, history: HistoryTx, notices: Notices) -> Arc<Recordings> {
        clear_ram_store(&cfg.ram_dir);
        let (list, note) = indexed;
        tracing::info!("{note}");
        let next_call = list.iter().map(|r| r.id + 1).max().unwrap_or(1);
        let count = list.len();
        let ring: Ring = Arc::new(Mutex::new(list.into()));
        let storage = Storage::start(cfg, ring.clone());
        storage.note_index(count, &note);
        let shared = Arc::new(Shared {
            ring,
            storage,
            history,
            notices,
            policy: Mutex::new(policy),
            counters: Counters::default(),
            recording_now: AtomicU64::new(0),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let recorder = Recorder { shared: shared.clone(), open: HashMap::new() };
        tokio::spawn(recorder.run(rx, audio.subscribe()));
        Arc::new(Recordings { shared, tx: RecorderTx(Some(tx)), next_call })
    }

    pub fn sender(&self) -> RecorderTx {
        self.tx.clone()
    }

    /// The first call id this run may use: past every recording's.
    pub fn next_call(&self) -> u64 {
        self.next_call
    }

    /// Newest first: one site's (`""` = those with no site) or all, at most `limit`; and how many
    /// that site has.
    pub fn list(&self, site: Option<&str>, limit: usize) -> (Vec<Recording>, usize) {
        let ring = lock(&self.shared.ring);
        let wanted = |r: &&Recording| site.is_none_or(|s| r.site == s);
        let items = ring.iter().rev().filter(wanted).take(limit).cloned().collect();
        (items, ring.iter().filter(wanted).count())
    }

    pub fn get(&self, id: u64) -> Option<Recording> {
        lock(&self.shared.ring).iter().find(|r| r.id == id).cloned()
    }

    /// Delete one recording, its file too.
    pub fn delete(&self, id: u64) -> bool {
        let mut ring = lock(&self.shared.ring);
        let Some(i) = ring.iter().position(|r| r.id == id) else { return false };
        if let Some(r) = ring.remove(i) {
            self.shared.storage.remove(&r);
            self.shared.history.recordings_gone(vec![r.file]);
        }
        true
    }

    /// Delete every recording of one store, or of both. Returns how many.
    pub fn clear(&self, store: Option<Store>) -> usize {
        let mut ring = lock(&self.shared.ring);
        let mut gone = Vec::new();
        ring.retain(|r| {
            let hit = store.is_none_or(|s| r.store == s);
            if hit {
                self.shared.storage.remove(r);
                gone.push(r.file.clone());
            }
            !hit
        });
        let n = gone.len();
        self.shared.history.recordings_gone(gone);
        n
    }

    pub fn policy(&self) -> Policy {
        self.shared.policy()
    }

    /// Apply a new policy; a lowered retention deletes at once. Returns how many were deleted.
    pub fn set_policy(&self, p: Policy) -> usize {
        *lock(&self.shared.policy) = p;
        if p.store == Store::Sd {
            self.shared.storage.request_probe();
        }
        let gone = storage::apply_retention(&mut lock(&self.shared.ring), &p.retention, &self.shared.storage);
        let n = gone.len();
        self.shared.history.recordings_gone(gone);
        n
    }

    pub fn summary(&self) -> Summary {
        let p = self.policy();
        let ((ram_n, ram_b), (sd_n, sd_b)) = storage::usage(&lock(&self.shared.ring));
        let c = &self.shared.counters;
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Summary {
            enabled: p.enabled,
            store: p.store,
            ram: StoreUsage { count: ram_n, bytes: ram_b },
            ram_max_count: p.retention.ram_max_count,
            sd: StoreUsage { count: sd_n, bytes: sd_b },
            sd_max_count: p.retention.sd_max_count,
            sd_max_bytes: p.retention.sd_max_bytes,
            sd_status: self.shared.storage.sd_status(),
            recording_now: get(&self.shared.recording_now) as usize,
            counters: CounterView {
                saved: get(&c.saved),
                no_audio: get(&c.no_audio),
                off: get(&c.off),
                failed: get(&c.failed),
                unmatched_chunks: get(&c.unmatched_chunks),
                missed_chunks: get(&c.missed_chunks),
            },
        }
    }

    /// Save the open recordings now and wait (up to `max`) for the card writes (shutdown).
    pub async fn flush(&self, max: Duration) {
        let deadline = tokio::time::Instant::now() + max;
        let (done, wait) = oneshot::channel();
        if self.tx.send(Input::Flush(done)) {
            let _ = tokio::time::timeout_at(deadline, wait).await;
        }
        while self.shared.storage.queue_jobs() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// RAM recordings of an earlier run: their ids are unknown to this one.
fn clear_ram_store(dir: &std::path::Path) {
    if let Err(e) = std::fs::create_dir_all(dir) {
        tracing::warn!("recordings: cannot create {}: {e}", dir.display());
        return;
    }
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "wav") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

struct Open {
    start: CallStart,
    /// Recording was on when the call opened.
    record: bool,
    pcm: Vec<i16>,
    voice: VoiceCounts,
    last_chunk: Instant,
    end: Option<(CallEnd, Instant)>,
}

struct Recorder {
    shared: Arc<Shared>,
    open: HashMap<u64, Open>,
}

impl Recorder {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Input>, mut audio: broadcast::Receiver<AudioChunk>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                input = rx.recv() => match input {
                    Some(Input::Start(s)) => self.start(s),
                    Some(Input::End(e)) => self.end(e),
                    Some(Input::Flush(done)) => {
                        for id in self.open.keys().copied().collect::<Vec<_>>() {
                            self.finish(id);
                        }
                        let _ = done.send(());
                    }
                    None => break,
                },
                chunk = audio.recv() => match chunk {
                    Ok(c) => self.chunk(&c),
                    Err(RecvError::Lagged(n)) => {
                        self.shared.counters.missed_chunks.fetch_add(n, Ordering::Relaxed);
                    }
                    Err(RecvError::Closed) => break,
                },
                _ = tick.tick() => self.tick(),
            }
            self.shared.recording_now.store(self.open.len() as u64, Ordering::Relaxed);
        }
    }

    fn start(&mut self, s: CallStart) {
        let p = self.shared.policy();
        let record = p.enabled && (p.every_call || s.record);
        self.open.entry(s.call).or_insert_with(|| Open {
            start: s,
            record,
            pcm: Vec::new(),
            voice: VoiceCounts::default(),
            last_chunk: Instant::now(),
            end: None,
        });
    }

    fn end(&mut self, e: CallEnd) {
        if let Some(o) = self.open.get_mut(&e.call) {
            o.end = Some((e, Instant::now()));
        }
    }

    fn chunk(&mut self, c: &AudioChunk) {
        let Some(o) = self.open.get_mut(&c.call) else {
            self.shared.counters.unmatched_chunks.fetch_add(1, Ordering::Relaxed);
            return;
        };
        o.voice.frames += 1;
        o.voice.errors += u64::from(c.error);
        o.voice.silent += u64::from(c.silent);
        if o.record {
            o.pcm.extend_from_slice(&c.pcm);
        }
        o.last_chunk = Instant::now();
    }

    fn tick(&mut self) {
        let due: Vec<u64> = self
            .open
            .iter()
            .filter(|(_, o)| match &o.end {
                Some((_, at)) => at.elapsed() >= DRAIN,
                None => o.last_chunk.elapsed() >= STALE,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.finish(id);
        }
    }

    fn finish(&mut self, id: u64) {
        let Some(o) = self.open.remove(&id) else { return };
        if o.voice.frames > 0 {
            self.shared.history.voice(VoiceResult {
                site: o.start.site.clone(),
                call_id: id,
                started_ms: o.start.started_unix_ms,
                frames: o.voice.frames,
                frame_errors: o.voice.errors,
            });
        }
        let counters = &self.shared.counters;
        if !o.record {
            counters.off.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if o.pcm.is_empty() {
            counters.no_audio.fetch_add(1, Ordering::Relaxed);
            return;
        }
        match save(&self.shared, o) {
            Ok(()) => {
                self.shared.notices.send(Notice::RecordingSaved { call: id });
                counters.saved.fetch_add(1, Ordering::Relaxed)
            }
            Err(e) => {
                tracing::warn!("recording {id} not saved: {e}");
                counters.failed.fetch_add(1, Ordering::Relaxed)
            }
        };
    }
}

/// Write (RAM) or queue (card) the call's WAV and list it.
fn save(shared: &Shared, o: Open) -> std::io::Result<()> {
    let s = &o.start;
    let (end_source, sources) = match o.end {
        Some((e, _)) => (e.source, e.sources),
        None => (None, s.source.into_iter().collect()),
    };
    let source = end_source.or(s.source);
    let file = index::file_name(s.started_unix_ms, s.call, s.tg, source, &s.site);
    let bytes = wav::wav_bytes(&o.pcm);
    let storage = &shared.storage;
    let policy = shared.policy();
    let to_card = policy.store == Store::Sd
        && match storage.sd_ready() {
            Ok(()) => true,
            Err(why) => {
                storage.note_fallback(&why);
                false
            }
        };
    let mut r = Recording {
        id: s.call,
        site: s.site.clone(),
        tg: s.tg,
        source,
        started_unix_ms: s.started_unix_ms,
        duration_ms: wav::duration_ms(bytes.len() as u64),
        bytes: bytes.len() as u64,
        file: file.clone(),
        store: Store::Ram,
        pending: None,
        sources,
        lane: Some(s.lane.number()),
        freq_hz: s.freq_hz,
        channel: s.channel.clone(),
        voice: Some(o.voice),
        path: storage.ram_dir().join(&file),
    };
    let queued = if to_card {
        let bytes = Arc::new(bytes);
        r.path = storage.sd_dir().join(&file);
        r.store = Store::Sd;
        r.pending = Some(bytes.clone());
        Some(bytes)
    } else {
        std::fs::create_dir_all(storage.ram_dir()).and_then(|_| std::fs::write(&r.path, &bytes))?;
        None
    };
    let path = r.path.clone();
    let row = row_of(&r);
    let gone = {
        // Listed before the write is queued, so the writer finds the entry it updates.
        let mut ring = lock(&shared.ring);
        ring.push_back(r);
        storage::apply_retention(&mut ring, &policy.retention, storage)
    };
    shared.history.recordings(vec![row]);
    shared.history.recordings_gone(gone);
    if let Some(bytes) = queued {
        storage.submit_sd_write(s.call, path, bytes);
    }
    Ok(())
}

fn row_of(r: &Recording) -> RecordingRow {
    RecordingRow {
        file: r.file.clone(),
        store: match r.store {
            Store::Ram => "ram".into(),
            Store::Sd => "sd".into(),
        },
        site: r.site.clone(),
        call_id: r.id,
        started_ms: r.started_unix_ms,
        tg: r.tg,
        source: r.source,
        bytes: r.bytes,
        duration_ms: r.duration_ms,
    }
}

/// Bring the history's list of recordings in line with the card's (boot): files new to it are
/// added and linked to their calls once, rows of files gone are removed. The card's recordings
/// get their calls' frequency, channel, lane and radios. Returns (added, removed).
pub fn reconcile(list: &mut [Recording], history: &HistoryStore) -> rusqlite::Result<(usize, usize)> {
    let known: std::collections::HashSet<String> = history.recording_files()?.into_iter().collect();
    let on_card: std::collections::HashSet<&str> = list.iter().map(|r| r.file.as_str()).collect();
    let new: Vec<RecordingRow> = list.iter().filter(|r| !known.contains(&r.file)).map(row_of).collect();
    let gone: Vec<String> = known.iter().filter(|f| !on_card.contains(f.as_str())).cloned().collect();
    let added = history.add_recordings(&new)?;
    let removed = history.remove_recordings(&gone)?;
    let files: Vec<String> = list.iter().map(|r| r.file.clone()).collect();
    let info = history.recording_info(&files)?;
    for r in list.iter_mut() {
        if let Some(i) = info.get(&r.file) {
            r.freq_hz = i.freq_hz;
            r.channel = i.channel.clone();
            r.lane = (i.lane > 0).then_some(i.lane);
            if !i.units.is_empty() {
                r.sources = i.units.clone();
            }
        }
    }
    Ok((added, removed))
}

#[cfg(test)]
mod tests;
