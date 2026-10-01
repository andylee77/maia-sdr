//! Where recordings live, RAM (tmpfs, lost on reboot) or the SD card, and the writer thread that
//! keeps the card's stalls off the recorder.
//!
//! The card (FAT32) stalls for seconds at a time (a 3.4 s write and a 6 s fsync were measured),
//! so nothing on the audio path touches it:
//!
//! - the recorder builds the WAV in memory and lists the recording at once. The entry holds the
//!   bytes until they are on the card, and playback serves them from RAM meanwhile;
//! - one thread writes `.<name>.part`, fsyncs, renames, then drops the RAM copy. Deletions of
//!   card files go through the same queue, behind the writes;
//! - a write that fails (card absent, read-only, full, I/O error) is saved to RAM instead, and the
//!   status says why;
//! - while the card is known bad, or more than `MAX_QUEUE_BYTES` are waiting, new recordings go
//!   to RAM.
//!
//! The thread also probes the card every 30 s (mounted, read-only, free space), so the API never
//! touches it. A store change never moves recordings: each store's retention deletes only its own
//! files.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::{index, Recording, Ring, Store};
use crate::util::time::unix_ms;

/// The RAM store (tmpfs).
pub const RAM_DIR: &str = "/tmp/p25_recordings";
/// SD writes waiting beyond this go to RAM instead (32 MB is ~35 min of voice: a stall far
/// longer than any measured).
pub const MAX_QUEUE_BYTES: u64 = 32 * 1024 * 1024;
/// Below this much free space the card counts as full.
pub const SD_MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;
/// Bound on the boot listing of the card (~2000 files on FAT took more than 3 s).
pub const INDEX_TIMEOUT: Duration = Duration::from_secs(15);
/// The card's partition: when it exists but is not mounted yet, boot waits for the mount.
const SD_DEVICE: &str = "/dev/mmcblk0p1";
pub const SD_MOUNT_WAIT: Duration = Duration::from_secs(20);
const PROBE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub ram_dir: PathBuf,
    pub sd_dir: PathBuf,
    /// Must be a mount before anything is written to `sd_dir` (without the card, `/mnt/sd` is a
    /// plain directory on the root filesystem). `None` skips the check.
    pub sd_mount: Option<PathBuf>,
    pub probe_interval: Duration,
    pub max_queue_bytes: u64,
    pub min_free_bytes: u64,
}

impl StorageConfig {
    pub fn board(sd_mount: &Path, sd_dir: PathBuf) -> Self {
        StorageConfig {
            ram_dir: PathBuf::from(RAM_DIR),
            sd_dir,
            sd_mount: cfg!(target_os = "linux").then(|| sd_mount.to_path_buf()),
            probe_interval: PROBE_INTERVAL,
            max_queue_bytes: MAX_QUEUE_BYTES,
            min_free_bytes: SD_MIN_FREE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SdState {
    /// Not probed yet.
    #[default]
    Unknown,
    Ok,
    /// Not mounted.
    Absent,
    ReadOnly,
    /// Under `SD_MIN_FREE_BYTES` free, or a write found no space.
    Full,
    /// The last write failed otherwise.
    Error,
}

impl SdState {
    pub fn as_str(self) -> &'static str {
        match self {
            SdState::Unknown => "unknown",
            SdState::Ok => "ok",
            SdState::Absent => "absent",
            SdState::ReadOnly => "read_only",
            SdState::Full => "full",
            SdState::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SdStatus {
    pub state: SdState,
    pub detail: Option<String>,
    pub dir: String,
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    pub writes_ok: u64,
    pub writes_failed: u64,
    /// Saved to RAM because the card was unusable or the write failed.
    pub fallbacks_to_ram: u64,
    pub deletes: u64,
    /// The newest and the slowest write (create, write, fsync, rename).
    pub last_write_ms: Option<u64>,
    pub max_write_ms: u64,
    pub last_error: Option<String>,
    pub last_probe_unix_ms: Option<u64>,
    pub indexed_at_boot: usize,
    pub index_note: String,
    pub queue_jobs: u64,
    pub queue_bytes: u64,
    /// How long the write in progress has taken, so a stall shows while it lasts.
    pub writing_for_ms: Option<u64>,
    pub writer_running: bool,
}

/// How many recordings each store keeps; the oldest go first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub ram_max_count: usize,
    pub sd_max_count: usize,
    pub sd_max_bytes: u64,
}

enum Job {
    Write { id: u64, path: PathBuf, bytes: Arc<Vec<u8>> },
    Delete { path: PathBuf },
    Probe,
}

pub struct Storage {
    cfg: StorageConfig,
    tx: Mutex<Option<Sender<Job>>>,
    status: Mutex<SdStatus>,
    queue_bytes: AtomicU64,
    queue_jobs: AtomicU64,
    /// Unix ms the write in progress started (0 = idle).
    write_started_ms: AtomicU64,
}

impl Storage {
    /// With no writer thread: card writes are refused (tests, a host with no card).
    pub fn new(cfg: StorageConfig) -> Arc<Self> {
        Arc::new(Storage {
            cfg,
            tx: Mutex::new(None),
            status: Mutex::new(SdStatus::default()),
            queue_bytes: AtomicU64::new(0),
            queue_jobs: AtomicU64::new(0),
            write_started_ms: AtomicU64::new(0),
        })
    }

    /// With the writer thread, which updates `ring` once a write lands (or falls back to RAM).
    pub fn start(cfg: StorageConfig, ring: Ring) -> Arc<Self> {
        let me = Self::new(cfg);
        let (tx, rx) = std::sync::mpsc::channel();
        *lock(&me.tx) = Some(tx);
        let w = me.clone();
        let spawned = std::thread::Builder::new().name("rec-writer".into()).spawn(move || w.writer_loop(rx, ring));
        if let Err(e) = spawned {
            tracing::warn!("recording writer not started: {e}");
            *lock(&me.tx) = None;
        }
        me
    }

    pub fn ram_dir(&self) -> &Path {
        &self.cfg.ram_dir
    }

    pub fn sd_dir(&self) -> &Path {
        &self.cfg.sd_dir
    }

    pub fn sd_status(&self) -> SdStatus {
        let mut st = lock(&self.status).clone();
        let started = self.write_started_ms.load(Ordering::Relaxed);
        st.dir = self.cfg.sd_dir.display().to_string();
        st.queue_jobs = self.queue_jobs();
        st.queue_bytes = self.queue_bytes.load(Ordering::Relaxed);
        st.writing_for_ms = (started != 0).then(|| unix_ms().saturating_sub(started));
        st.writer_running = self.writer_running();
        st
    }

    pub fn sd_state(&self) -> SdState {
        lock(&self.status).state
    }

    pub fn queue_jobs(&self) -> u64 {
        self.queue_jobs.load(Ordering::Relaxed)
    }

    fn writer_running(&self) -> bool {
        lock(&self.tx).is_some()
    }

    pub fn note_index(&self, count: usize, note: &str) {
        let mut g = lock(&self.status);
        g.indexed_at_boot = count;
        g.index_note = note.to_string();
    }

    /// `Ok` when a new recording may be queued for the card, else why it goes to RAM instead.
    /// Never touches the card.
    pub fn sd_ready(&self) -> Result<(), String> {
        if !self.writer_running() {
            return Err("sd writer not running".into());
        }
        let state = self.sd_state();
        if !matches!(state, SdState::Ok | SdState::Unknown) {
            return Err(format!("sd {}", state.as_str()));
        }
        if self.queue_bytes.load(Ordering::Relaxed) >= self.cfg.max_queue_bytes {
            return Err("sd write queue full (card stalled)".into());
        }
        Ok(())
    }

    /// Queue the card write of recording `id`.
    pub fn submit_sd_write(&self, id: u64, path: PathBuf, bytes: Arc<Vec<u8>>) {
        let len = bytes.len() as u64;
        self.queue_bytes.fetch_add(len, Ordering::Relaxed);
        self.queue_jobs.fetch_add(1, Ordering::Relaxed);
        if !self.send(Job::Write { id, path, bytes }) {
            self.queue_bytes.fetch_sub(len, Ordering::Relaxed);
            self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// A recording saved to RAM although the card was selected.
    pub fn note_fallback(&self, why: &str) {
        let mut g = lock(&self.status);
        g.fallbacks_to_ram += 1;
        g.last_error = Some(why.to_string());
    }

    /// Delete a recording's file: at once in RAM (tmpfs never stalls), behind the pending
    /// writes on the card.
    pub fn remove(&self, r: &Recording) {
        match r.store {
            Store::Sd => {
                self.queue_jobs.fetch_add(1, Ordering::Relaxed);
                if !self.send(Job::Delete { path: r.path.clone() }) {
                    self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
                }
            }
            Store::Ram => {
                let _ = std::fs::remove_file(&r.path);
            }
        }
    }

    /// Probe the card now (the store setting just changed to SD).
    pub fn request_probe(&self) {
        self.send(Job::Probe);
    }

    fn send(&self, job: Job) -> bool {
        lock(&self.tx).as_ref().is_some_and(|tx| tx.send(job).is_ok())
    }

    fn writer_loop(self: Arc<Self>, rx: Receiver<Job>, ring: Ring) {
        self.probe(true);
        let mut last_probe = Instant::now();
        loop {
            match rx.recv_timeout(self.cfg.probe_interval) {
                Ok(Job::Write { id, path, bytes }) => {
                    self.write(id, &path, &bytes, &ring);
                    self.queue_bytes.fetch_sub(bytes.len() as u64, Ordering::Relaxed);
                    self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
                }
                Ok(Job::Delete { path }) => {
                    if std::fs::remove_file(&path).is_ok() {
                        lock(&self.status).deletes += 1;
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

    fn write(&self, id: u64, path: &Path, bytes: &Arc<Vec<u8>>, ring: &Ring) {
        let t0 = Instant::now();
        self.write_started_ms.store(unix_ms().max(1), Ordering::Relaxed);
        let res = write_durable(path, bytes);
        self.write_started_ms.store(0, Ordering::Relaxed);
        let ms = t0.elapsed().as_millis() as u64;
        match res {
            Ok(()) => {
                {
                    let mut g = lock(&self.status);
                    g.writes_ok += 1;
                    g.last_write_ms = Some(ms);
                    g.max_write_ms = g.max_write_ms.max(ms);
                    if matches!(g.state, SdState::Error | SdState::Unknown) {
                        g.state = SdState::Ok;
                        g.detail = None;
                    }
                }
                if let Some(r) = lock(ring).iter_mut().find(|r| r.id == id) {
                    r.pending = None;
                }
            }
            Err(err) => {
                let msg = format!("write {}: {err}", path.display());
                tracing::warn!("recording {id}: SD {msg}; saving to RAM instead");
                {
                    let mut g = lock(&self.status);
                    g.writes_failed += 1;
                    g.fallbacks_to_ram += 1;
                    g.last_write_ms = Some(ms);
                    g.max_write_ms = g.max_write_ms.max(ms);
                    g.state = classify(&err);
                    g.detail = Some(msg.clone());
                    g.last_error = Some(msg);
                }
                let ram = self.cfg.ram_dir.join(path.file_name().unwrap_or_default());
                let ram_ok = std::fs::create_dir_all(&self.cfg.ram_dir).and_then(|_| std::fs::write(&ram, bytes.as_slice())).is_ok();
                // When RAM fails too, the copy in `pending` keeps the recording playable.
                if let Some(r) = lock(ring).iter_mut().find(|r| r.id == id).filter(|_| ram_ok) {
                    r.path = ram;
                    r.store = Store::Ram;
                    r.pending = None;
                }
                // A transient error, or a card that came back, is ok again at once.
                self.probe(false);
            }
        }
    }

    fn probe(&self, initial: bool) {
        let (state, detail, total, free) = probe_sd(&self.cfg);
        if initial && state == SdState::Ok {
            remove_stale_parts(&self.cfg.sd_dir);
        }
        let mut g = lock(&self.status);
        g.state = state;
        g.detail = detail;
        g.total_bytes = total;
        g.free_bytes = free;
        g.last_probe_unix_ms = Some(unix_ms());
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Create `.<name>.part`, write, fsync, rename.
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

fn classify(e: &std::io::Error) -> SdState {
    // Linux errno: EROFS 30, ENOSPC 28, ENOENT 2, ENXIO 6, ENODEV 19.
    if cfg!(target_os = "linux") {
        match e.raw_os_error() {
            Some(30) => return SdState::ReadOnly,
            Some(28) => return SdState::Full,
            Some(2) | Some(6) | Some(19) => return SdState::Absent,
            _ => {}
        }
    }
    SdState::Error
}

/// Wait (blocking, up to `max`) for the card to be mounted when it is there: at boot it can be
/// mounted after the scanner starts. True when mounted.
pub fn wait_for_sd(cfg: &StorageConfig, max: Duration) -> bool {
    let Some(m) = cfg.sd_mount.as_deref() else { return false };
    let t0 = Instant::now();
    loop {
        if mount_state(m).is_some() {
            return true;
        }
        if !Path::new(SD_DEVICE).exists() || t0.elapsed() >= max {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The recordings already on the card, oldest first, and a note for the status.
pub fn index(cfg: &StorageConfig) -> (Vec<Recording>, String) {
    if let Some(m) = cfg.sd_mount.as_deref() {
        if mount_state(m).is_none() {
            return (Vec::new(), format!("{} not mounted", m.display()));
        }
    }
    index::list(&cfg.sd_dir)
}

/// Whether `mount` is mounted, and if so read-only, from `/proc/mounts`.
fn mount_state(mount: &Path) -> Option<bool> {
    let text = std::fs::read_to_string("/proc/mounts").ok()?;
    let want = mount.to_str()?;
    text.lines().find_map(|l| {
        let f: Vec<&str> = l.split_whitespace().collect();
        (f.len() >= 4 && f[1] == want).then(|| f[3].split(',').any(|o| o == "ro"))
    })
}

/// (total, free) bytes of the filesystem holding `path`.
fn fs_space(path: &Path) -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        let Some(cpath) = path.to_str().and_then(|p| std::ffi::CString::new(p).ok()) else { return (None, None) };
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `cpath` is a valid C string and `s` a writable statvfs.
        if unsafe { libc::statvfs(cpath.as_ptr(), &mut s) } == 0 {
            let frsize = s.f_frsize as u64;
            return (Some(s.f_blocks as u64 * frsize), Some(s.f_bavail as u64 * frsize));
        }
    }
    let _ = path;
    (None, None)
}

fn probe_sd(cfg: &StorageConfig) -> (SdState, Option<String>, Option<u64>, Option<u64>) {
    if let Some(m) = cfg.sd_mount.as_deref() {
        match mount_state(m) {
            None => return (SdState::Absent, Some(format!("{} is not mounted", m.display())), None, None),
            Some(true) => return (SdState::ReadOnly, Some(format!("{} is mounted read-only", m.display())), None, None),
            Some(false) => {}
        }
    }
    if let Err(e) = std::fs::create_dir_all(&cfg.sd_dir) {
        return (classify(&e), Some(format!("mkdir {}: {e}", cfg.sd_dir.display())), None, None);
    }
    let (total, free) = fs_space(&cfg.sd_dir);
    if free.is_some_and(|f| f < cfg.min_free_bytes) {
        return (SdState::Full, Some(format!("less than {} MB free", cfg.min_free_bytes >> 20)), total, free);
    }
    (SdState::Ok, None, total, free)
}

/// Remove the `.part` files of writes a crash or power cut interrupted.
fn remove_stale_parts(dir: &Path) {
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let n = e.file_name();
        let n = n.to_string_lossy();
        if n.starts_with('.') && n.ends_with(".part") {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Positions (ascending) of the recordings beyond each store's retention: oldest first, and the
/// newest recording of a store is always kept.
pub fn evictions(ring: &VecDeque<Recording>, r: &Retention) -> Vec<usize> {
    let (mut ram_n, mut sd_n, mut sd_bytes) = (0usize, 0usize, 0u64);
    let mut out = Vec::new();
    for (i, e) in ring.iter().enumerate().rev() {
        match e.store {
            Store::Sd => {
                sd_n += 1;
                sd_bytes = sd_bytes.saturating_add(e.bytes);
                if sd_n > r.sd_max_count.max(1) || (sd_n > 1 && sd_bytes > r.sd_max_bytes) {
                    out.push(i);
                }
            }
            Store::Ram => {
                ram_n += 1;
                if ram_n > r.ram_max_count.max(1) {
                    out.push(i);
                }
            }
        }
    }
    out.sort_unstable();
    out
}

/// (count, bytes) of each store: (RAM, SD).
pub fn usage(ring: &VecDeque<Recording>) -> ((usize, u64), (usize, u64)) {
    ring.iter().fold(((0, 0), (0, 0)), |((rn, rb), (sn, sb)), e| match e.store {
        Store::Sd => ((rn, rb), (sn + 1, sb + e.bytes)),
        Store::Ram => ((rn + 1, rb + e.bytes), (sn, sb)),
    })
}

/// Drop the recordings beyond the retention and delete their files. Returns how many.
pub fn apply_retention(ring: &mut VecDeque<Recording>, retention: &Retention, storage: &Storage) -> usize {
    let idx = evictions(ring, retention);
    for &i in idx.iter().rev() {
        if let Some(old) = ring.remove(i) {
            storage.remove(&old);
        }
    }
    idx.len()
}
