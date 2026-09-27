//! Change 057: where call recordings live — RAM (tmpfs, the pre-057
//! store) or the SD card — and the writer thread that keeps SD stalls
//! off the recorder / audio / decode path.
//!
//! The SD partition (`/mnt/sd`, FAT32, ~58 GB free on the bench cards)
//! has multi-second write stalls: the bench measured a 3.4 s write and a
//! 6 s fsync. So nothing on the hot path touches it:
//!
//!   - the recorder builds the WAV in memory, lists it at once (the
//!     entry holds the bytes, `RecordingEntry::pending`, and
//!     `/api/recordings/{id}.wav` serves them from RAM), and queues the
//!     write here;
//!   - one OS thread (`p25-rec-writer`) writes `.<name>.part`, fsyncs,
//!     renames, then drops the RAM copy from the entry. Deletions of SD
//!     files (retention) go through the same queue;
//!   - a write that fails (card absent, read-only, full, I/O error) is
//!     written to the RAM store instead, so the recording survives
//!     until reboot, and the status says why;
//!   - while the card is known bad, or more than `MAX_QUEUE_BYTES` are
//!     waiting (a stall far beyond the measured ones), new recordings go
//!     straight to RAM.
//!
//! The writer thread also probes the card every `PROBE_INTERVAL`
//! (mounted? read-only? free space via statvfs) so HTTP handlers never
//! touch the card to report its state.
//!
//! Existing recordings never move when the store setting changes: new
//! recordings go to the new store, old ones stay listed and playable
//! where they are, and each store's retention only deletes its own
//! files. SD recordings survive a restart: `index_sd` lists them at
//! boot (from the file names) and call ids continue after the highest
//! one found, so ids stay unique across restarts.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::audio::recorder::{RecordingEntry, RecordingStore};
use crate::services::ui_settings::Retention;

/// RAM store (tmpfs, cleared at start-up and lost on reboot).
pub const RAM_DIR: &str = "/tmp/p25_recordings";
/// SD partition mount point and the recordings directory on it.
pub const SD_MOUNT: &str = "/mnt/sd";
pub const SD_DIR: &str = "/mnt/sd/p25_recordings";

/// SD writes waiting in RAM beyond this go to the RAM store instead
/// (32 MB ≈ 35 min of voice: a stall far longer than the 6 s measured).
pub const MAX_QUEUE_BYTES: u64 = 32 * 1024 * 1024;
/// Below this much free space the card counts as full.
pub const SD_MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;
/// Upper bound on the boot-time listing of the SD directory.
pub const INDEX_TIMEOUT: Duration = Duration::from_secs(3);
/// Card probe cadence (mount, read-only, free space).
const PROBE_INTERVAL: Duration = Duration::from_secs(30);

pub const STORE_RAM: &str = "ram";
pub const STORE_SD: &str = "sd";

#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub ram_dir: PathBuf,
    pub sd_dir: PathBuf,
    /// Mount point that must be listed in `/proc/mounts` before anything
    /// is written to `sd_dir` (without the card, `/mnt/sd` is a plain
    /// directory on the root filesystem). `None` skips the check.
    pub sd_mount: Option<PathBuf>,
    pub probe_interval: Duration,
    pub max_queue_bytes: u64,
    pub min_free_bytes: u64,
}

impl StorageConfig {
    /// The board layout; `P25_REC_RAM_DIR` / `P25_REC_SD_DIR` override
    /// the directories (bench experiments).
    pub fn board() -> Self {
        let env = |k: &str, d: &str| {
            std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.to_string())
        };
        StorageConfig {
            ram_dir: PathBuf::from(env("P25_REC_RAM_DIR", RAM_DIR)),
            sd_dir: PathBuf::from(env("P25_REC_SD_DIR", SD_DIR)),
            sd_mount: cfg!(target_os = "linux").then(|| PathBuf::from(SD_MOUNT)),
            probe_interval: PROBE_INTERVAL,
            max_queue_bytes: MAX_QUEUE_BYTES,
            min_free_bytes: SD_MIN_FREE_BYTES,
        }
    }
}

/// SD store health, as reported by `/api/ui/settings`
/// `recording_storage.sd`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SdStatus {
    /// "unknown" (not probed yet), "ok", "absent" (not mounted),
    /// "read_only", "full" (< `SD_MIN_FREE_BYTES` free or ENOSPC),
    /// "error" (the last write failed otherwise).
    pub state: &'static str,
    pub detail: Option<String>,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub writes_ok: u64,
    pub writes_failed: u64,
    /// Recordings written to RAM because the SD write failed or the card
    /// was unusable when the recording was saved.
    pub fallbacks_to_ram: u64,
    pub deletes: u64,
    /// Duration of the newest / slowest SD write (create + write +
    /// fsync + rename).
    pub last_write_ms: Option<u64>,
    pub max_write_ms: u64,
    pub last_error: Option<String>,
    pub last_probe_unix_ms: Option<u64>,
    /// Recordings found on the card at start-up (`index_sd`).
    pub indexed_at_boot: usize,
    pub index_note: String,
}

impl Default for SdStatus {
    fn default() -> Self {
        SdStatus {
            state: "unknown",
            detail: None,
            total_bytes: None,
            free_bytes: None,
            writes_ok: 0,
            writes_failed: 0,
            fallbacks_to_ram: 0,
            deletes: 0,
            last_write_ms: None,
            max_write_ms: 0,
            last_error: None,
            last_probe_unix_ms: None,
            indexed_at_boot: 0,
            index_note: String::new(),
        }
    }
}

enum Job {
    Write { id: u64, path: PathBuf, bytes: Arc<Vec<u8>> },
    Delete { path: PathBuf },
    Probe,
}

/// Handle shared by the recorder, the HTTP handlers and the writer
/// thread.
pub struct RecordingStorage {
    cfg: StorageConfig,
    tx: Mutex<Option<Sender<Job>>>,
    status: Mutex<SdStatus>,
    queue_bytes: AtomicU64,
    queue_jobs: AtomicU64,
    /// Unix ms at which the in-flight SD write started (0 = idle), so a
    /// stall is visible while it happens.
    write_started_ms: AtomicU64,
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl RecordingStorage {
    /// Handle without a writer thread (SD writes are refused until
    /// `start` is used). Tests and the host build without a card.
    pub fn new(cfg: StorageConfig) -> Arc<Self> {
        Arc::new(RecordingStorage {
            cfg,
            tx: Mutex::new(None),
            status: Mutex::new(SdStatus::default()),
            queue_bytes: AtomicU64::new(0),
            queue_jobs: AtomicU64::new(0),
            write_started_ms: AtomicU64::new(0),
        })
    }

    /// Handle plus the writer thread. `store` is the recordings ring the
    /// writer updates once a write lands (or falls back to RAM).
    pub fn start(cfg: StorageConfig, store: RecordingStore) -> Arc<Self> {
        let me = Self::new(cfg);
        let (tx, rx) = std::sync::mpsc::channel();
        if let Ok(mut g) = me.tx.lock() {
            *g = Some(tx);
        }
        let w = me.clone();
        let spawned = std::thread::Builder::new()
            .name("p25-rec-writer".into())
            .spawn(move || w.writer_loop(rx, store));
        if let Err(e) = spawned {
            tracing::warn!("recording writer thread not started: {e}");
            if let Ok(mut g) = me.tx.lock() {
                *g = None;
            }
        }
        me
    }

    pub fn config(&self) -> &StorageConfig {
        &self.cfg
    }

    pub fn ram_dir(&self) -> &Path {
        &self.cfg.ram_dir
    }

    pub fn sd_dir(&self) -> &Path {
        &self.cfg.sd_dir
    }

    /// Current SD status plus the live queue figures.
    pub fn sd_status(&self) -> serde_json::Value {
        let st = self.status.lock().map(|g| g.clone()).unwrap_or_default();
        let started = self.write_started_ms.load(Ordering::Relaxed);
        let mut v = serde_json::to_value(&st).unwrap_or_default();
        if let Some(o) = v.as_object_mut() {
            o.insert("dir".into(), self.cfg.sd_dir.display().to_string().into());
            o.insert("queue_jobs".into(), self.queue_jobs.load(Ordering::Relaxed).into());
            o.insert("queue_bytes".into(), self.queue_bytes.load(Ordering::Relaxed).into());
            o.insert(
                "writing_for_ms".into(),
                (started != 0).then(|| now_unix_ms().saturating_sub(started)).into(),
            );
            o.insert("writer_running".into(), self.writer_running().into());
        }
        v
    }

    pub fn sd_state(&self) -> &'static str {
        self.status.lock().map(|g| g.state).unwrap_or("unknown")
    }

    fn writer_running(&self) -> bool {
        self.tx.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Record the boot-time index result in the status.
    pub fn note_index(&self, count: usize, note: &str) {
        if let Ok(mut g) = self.status.lock() {
            g.indexed_at_boot = count;
            g.index_note = note.to_string();
        }
    }

    /// `Ok` when a new recording may be queued for the SD card, else
    /// why it goes to RAM instead. Never touches the card.
    pub fn sd_ready(&self) -> Result<(), String> {
        if !self.writer_running() {
            return Err("sd writer not running".into());
        }
        let state = self.sd_state();
        if !matches!(state, "ok" | "unknown") {
            return Err(format!("sd {state}"));
        }
        if self.queue_bytes.load(Ordering::Relaxed) >= self.cfg.max_queue_bytes {
            return Err("sd write queue full (card stalled)".into());
        }
        Ok(())
    }

    /// Queue an SD write of `bytes` to `path` for recording `id`.
    pub fn submit_sd_write(&self, id: u64, path: PathBuf, bytes: Arc<Vec<u8>>) {
        let len = bytes.len() as u64;
        self.queue_bytes.fetch_add(len, Ordering::Relaxed);
        self.queue_jobs.fetch_add(1, Ordering::Relaxed);
        let sent = self
            .tx
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|tx| tx.send(Job::Write { id, path, bytes }).is_ok()))
            .unwrap_or(false);
        if !sent {
            self.queue_bytes.fetch_sub(len, Ordering::Relaxed);
            self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Count a recording saved to RAM although the SD store was
    /// selected.
    pub fn note_fallback(&self, why: &str) {
        if let Ok(mut g) = self.status.lock() {
            g.fallbacks_to_ram += 1;
            g.last_error = Some(why.to_string());
        }
    }

    /// Delete a recording's file: inline for RAM (tmpfs never stalls),
    /// queued behind pending writes for the SD card.
    pub fn remove(&self, e: &RecordingEntry) {
        if e.storage == STORE_SD {
            let queued = self
                .tx
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|tx| tx.send(Job::Delete { path: e.path.clone() }).is_ok()))
                .unwrap_or(false);
            if queued {
                self.queue_jobs.fetch_add(1, Ordering::Relaxed);
            }
        } else {
            let _ = std::fs::remove_file(&e.path);
        }
    }

    /// Ask the writer for a fresh probe (e.g. the store setting just
    /// changed to SD).
    pub fn request_probe(&self) {
        if let Ok(g) = self.tx.lock() {
            if let Some(tx) = g.as_ref() {
                let _ = tx.send(Job::Probe);
            }
        }
    }

    // ── writer thread ────────────────────────────────────────────────

    fn writer_loop(self: Arc<Self>, rx: Receiver<Job>, store: RecordingStore) {
        self.probe(true);
        let mut last_probe = Instant::now();
        loop {
            let job = rx.recv_timeout(self.cfg.probe_interval);
            match job {
                Ok(Job::Write { id, path, bytes }) => {
                    self.do_write(id, &path, &bytes, &store);
                    self.queue_bytes.fetch_sub(bytes.len() as u64, Ordering::Relaxed);
                    self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
                }
                Ok(Job::Delete { path }) => {
                    if std::fs::remove_file(&path).is_ok() {
                        if let Ok(mut g) = self.status.lock() {
                            g.deletes += 1;
                        }
                    }
                    self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
                }
                Ok(Job::Probe) | Err(RecvTimeoutError::Timeout) => {
                    self.probe(false);
                    last_probe = Instant::now();
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if last_probe.elapsed() >= self.cfg.probe_interval {
                self.probe(false);
                last_probe = Instant::now();
            }
        }
    }

    fn do_write(&self, id: u64, path: &Path, bytes: &Arc<Vec<u8>>, store: &RecordingStore) {
        let t0 = Instant::now();
        self.write_started_ms.store(now_unix_ms().max(1), Ordering::Relaxed);
        let res = write_durable(path, bytes);
        self.write_started_ms.store(0, Ordering::Relaxed);
        let ms = t0.elapsed().as_millis() as u64;
        match res {
            Ok(()) => {
                if let Ok(mut g) = self.status.lock() {
                    g.writes_ok += 1;
                    g.last_write_ms = Some(ms);
                    g.max_write_ms = g.max_write_ms.max(ms);
                    if g.state == "error" || g.state == "unknown" {
                        g.state = "ok";
                        g.detail = None;
                    }
                }
                let mut ring = store.blocking_lock();
                if let Some(e) = ring.iter_mut().find(|e| e.id == id) {
                    e.pending = None;
                }
            }
            Err(err) => {
                let state = classify(&err);
                let msg = format!("write {}: {err}", path.display());
                tracing::warn!("recording {id}: SD {msg}; saving to RAM instead");
                if let Ok(mut g) = self.status.lock() {
                    g.writes_failed += 1;
                    g.fallbacks_to_ram += 1;
                    g.last_write_ms = Some(ms);
                    g.max_write_ms = g.max_write_ms.max(ms);
                    g.state = state;
                    g.detail = Some(msg.clone());
                    g.last_error = Some(msg);
                }
                let name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
                let ram = self.cfg.ram_dir.join(name);
                let ram_ok = std::fs::create_dir_all(&self.cfg.ram_dir)
                    .and_then(|_| std::fs::write(&ram, bytes.as_slice()))
                    .is_ok();
                let mut ring = store.blocking_lock();
                if let Some(e) = ring.iter_mut().find(|e| e.id == id) {
                    if ram_ok {
                        e.path = ram;
                        e.storage = STORE_RAM;
                        e.pending = None;
                    }
                    // else: the RAM copy in `pending` keeps it playable.
                }
                drop(ring);
                // A card that came back (or a transient error) is ok
                // again at the next probe; re-check now.
                self.probe(false);
            }
        }
    }

    fn probe(&self, initial: bool) {
        let (state, detail, total, free) = probe_sd(&self.cfg);
        if initial && state == "ok" {
            remove_stale_parts(&self.cfg.sd_dir);
        }
        if let Ok(mut g) = self.status.lock() {
            // A probe that finds the card fine clears a write error; a
            // bad probe always wins.
            g.state = state;
            g.detail = detail;
            g.total_bytes = total;
            g.free_bytes = free;
            g.last_probe_unix_ms = Some(now_unix_ms());
        }
    }
}

/// create `.<name>.part` + write + fsync + rename.
pub fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name"))?;
    let part = path.with_file_name(format!(".{name}.part"));
    let res = (|| {
        let mut f = std::fs::File::create(&part)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&part, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    res
}

fn classify(e: &std::io::Error) -> &'static str {
    // Linux errno: EROFS 30, ENOSPC 28, ENOENT 2, ENXIO 6, ENODEV 19.
    if cfg!(target_os = "linux") {
        match e.raw_os_error() {
            Some(30) => return "read_only",
            Some(28) => return "full",
            Some(2) | Some(6) | Some(19) => return "absent",
            _ => {}
        }
    }
    "error"
}

/// (mounted, read-only) for `mount` from `/proc/mounts`.
fn mount_state(mount: &Path) -> Option<bool> {
    let text = std::fs::read_to_string("/proc/mounts").ok()?;
    let want = mount.to_str()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 4 && f[1] == want).then(|| f[3].split(',').any(|o| o == "ro"))
    })
}

fn fs_space(path: &Path) -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        if let Some((total, free)) = path.to_str().and_then(crate::httpd::api::system::fs_usage) {
            return (Some(total), Some(free));
        }
    }
    let _ = path;
    (None, None)
}

/// Probe the card: (state, detail, total bytes, free bytes).
fn probe_sd(cfg: &StorageConfig) -> (&'static str, Option<String>, Option<u64>, Option<u64>) {
    if let Some(m) = cfg.sd_mount.as_deref() {
        match mount_state(m) {
            None => return ("absent", Some(format!("{} is not mounted", m.display())), None, None),
            Some(true) => {
                return ("read_only", Some(format!("{} is mounted read-only", m.display())), None, None)
            }
            Some(false) => {}
        }
    }
    if let Err(e) = std::fs::create_dir_all(&cfg.sd_dir) {
        return (classify(&e), Some(format!("mkdir {}: {e}", cfg.sd_dir.display())), None, None);
    }
    let (total, free) = fs_space(&cfg.sd_dir);
    if free.is_some_and(|f| f < cfg.min_free_bytes) {
        return ("full", Some(format!("less than {} MB free", cfg.min_free_bytes >> 20)), total, free);
    }
    ("ok", None, total, free)
}

/// Remove `.<name>.part` leftovers of writes interrupted by a crash or a
/// power cut.
fn remove_stale_parts(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if n.starts_with('.') && n.ends_with(".part") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// (started_unix_ms, id, talkgroup, source) from
/// `rec_<ms>_<id>_tg<tg>[_from<src>].wav`.
pub fn parse_filename(name: &str) -> Option<(u64, u64, u16, Option<u32>)> {
    let body = name.strip_prefix("rec_")?.strip_suffix(".wav")?;
    let mut it = body.split('_');
    let ms = it.next()?.parse().ok()?;
    let id = it.next()?.parse().ok()?;
    let tg = it.next()?.strip_prefix("tg")?.parse().ok()?;
    let src = match it.next() {
        Some(s) => Some(s.strip_prefix("from")?.parse().ok()?),
        None => None,
    };
    if it.next().is_some() {
        return None;
    }
    Some((ms, id, tg, src))
}

/// List the recordings already on the card (boot). Returns the entries
/// sorted by id and a note for the status. Reads the directory only:
/// per-call details beyond the file name (frequency, counters) are not
/// kept across a restart.
pub fn index_sd(cfg: &StorageConfig) -> (Vec<RecordingEntry>, String) {
    if let Some(m) = cfg.sd_mount.as_deref() {
        match mount_state(m) {
            None => return (Vec::new(), format!("{} not mounted", m.display())),
            Some(_) => {}
        }
    }
    let rd = match std::fs::read_dir(&cfg.sd_dir) {
        Ok(rd) => rd,
        Err(e) => return (Vec::new(), format!("{}: {e}", cfg.sd_dir.display())),
    };
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some((started, id, tg, source)) = parse_filename(&name) else {
            skipped += usize::from(name.ends_with(".wav"));
            continue;
        };
        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(RecordingEntry {
            id,
            talkgroup: tg,
            source,
            started_unix_ms: started,
            // 8 kHz 16-bit mono: 16 bytes per ms after the 44-byte header.
            duration_ms: size.saturating_sub(44) / 16,
            path: e.path(),
            size_bytes: size,
            filename: name,
            sources_observed: source.into_iter().collect(),
            storage: STORE_SD,
            ..Default::default()
        });
    }
    out.sort_by_key(|e| e.id);
    // Two files with one id (a restart whose index timed out): keep the
    // newer one listed; the other stays on the card untouched.
    out.dedup_by(|b, a| {
        if a.id == b.id {
            if b.started_unix_ms > a.started_unix_ms {
                std::mem::swap(a, b);
            }
            true
        } else {
            false
        }
    });
    let note = format!(
        "indexed {} recording(s) in {}{}",
        out.len(),
        cfg.sd_dir.display(),
        if skipped > 0 { format!(" ({skipped} unrecognised .wav names skipped)") } else { String::new() },
    );
    (out, note)
}

/// Indices (ring order, ascending) of the recordings to delete under
/// `r`: beyond `ram_max_count` RAM recordings, beyond `sd_max_count`
/// SD recordings or `sd_max_bytes` of them. Oldest first; the newest
/// recording of a store is always kept.
pub fn evictions(ring: &VecDeque<RecordingEntry>, r: &Retention) -> Vec<usize> {
    let (mut ram_n, mut sd_n, mut sd_bytes) = (0usize, 0usize, 0u64);
    let mut out = Vec::new();
    for (i, e) in ring.iter().enumerate().rev() {
        if e.storage == STORE_SD {
            sd_n += 1;
            sd_bytes = sd_bytes.saturating_add(e.size_bytes);
            if sd_n > r.sd_max_count.max(1) || (sd_n > 1 && sd_bytes > r.sd_max_bytes) {
                out.push(i);
            }
        } else {
            ram_n += 1;
            if ram_n > r.ram_max_count.max(1) {
                out.push(i);
            }
        }
    }
    out.sort_unstable();
    out
}

/// (count, bytes) of each store in the ring: (ram, sd).
pub fn usage(ring: &VecDeque<RecordingEntry>) -> ((usize, u64), (usize, u64)) {
    ring.iter().fold(((0, 0), (0, 0)), |((rn, rb), (sn, sb)), e| {
        if e.storage == STORE_SD {
            ((rn, rb), (sn + 1, sb + e.size_bytes))
        } else {
            ((rn + 1, rb + e.size_bytes), (sn, sb))
        }
    })
}

#[cfg(test)]
#[path = "rec_storage_tests.rs"]
mod tests;
