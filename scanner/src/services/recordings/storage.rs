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
//! files. Locking a card recording moves its file to the locked tree through the same queue.
//!
//! Retention: the card keeps recordings for days (longer for locked ones) up to a size, which
//! takes the oldest unlocked first; RAM keeps a count (`evictions`).

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::{index, Recording, Ring, Store};
use crate::services::clock::site::MIN_VALID_MS;
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
    /// Mounted with space, but the last write failed (a full directory, for one): new recordings
    /// still try the card, and the next write that lands clears it.
    Failing,
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
            SdState::Failing => "failing",
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

/// What each store keeps; the oldest go first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub ram_max_count: usize,
    /// Days the card keeps a recording, and a locked one.
    pub sd_keep_days: u32,
    pub locked_keep_days: u32,
    pub sd_max_bytes: u64,
    /// Recordings in one day folder.
    pub folder_max_count: usize,
}

/// Where a locked or unlocked recording's file moved, or why it could not.
pub type MoveReply = tokio::sync::oneshot::Sender<Result<(), String>>;

enum Job {
    Write { id: u64, path: PathBuf, bytes: Arc<Vec<u8>> },
    /// `folder` too, when it holds no other recording.
    Delete { path: PathBuf, folder: Option<PathBuf> },
    /// Recording `id`'s file to `to` (locked or not), its entry updated once it is there.
    Move { id: u64, to: PathBuf, locked: bool, reply: MoveReply },
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
    /// The last write failed: a probe that finds the card fine says `failing` until one lands.
    write_failed: AtomicBool,
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
            write_failed: AtomicBool::new(false),
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
        if !matches!(state, SdState::Ok | SdState::Unknown | SdState::Failing) {
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
    /// writes on the card, with its folder when `last_in_folder`.
    pub fn remove(&self, r: &Recording, last_in_folder: bool) {
        match r.store {
            Store::Sd => {
                let folder = last_in_folder.then(|| r.path.parent().map(Path::to_path_buf)).flatten();
                self.queue_jobs.fetch_add(1, Ordering::Relaxed);
                if !self.send(Job::Delete { path: r.path.clone(), folder }) {
                    self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
                }
            }
            Store::Ram => {
                let _ = std::fs::remove_file(&r.path);
            }
        }
    }

    /// Move card recording `id`'s file to `to`, behind its own write if that is still queued. The
    /// writer updates its entry (path, locked) once it is there and answers on `reply`; false when
    /// no writer runs.
    pub fn submit_move(&self, id: u64, to: PathBuf, locked: bool, reply: MoveReply) -> bool {
        self.queue_jobs.fetch_add(1, Ordering::Relaxed);
        let sent = self.send(Job::Move { id, to, locked, reply });
        if !sent {
            self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
        }
        sent
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
                Ok(Job::Delete { path, folder }) => {
                    if std::fs::remove_file(&path).is_ok() {
                        lock(&self.status).deletes += 1;
                    }
                    // Fails, harmlessly, when a file the list does not know is left in it.
                    if let Some(f) = folder {
                        let _ = std::fs::remove_dir(f);
                    }
                    self.queue_jobs.fetch_sub(1, Ordering::Relaxed);
                }
                Ok(Job::Move { id, to, locked, reply }) => {
                    let _ = reply.send(self.relocate(id, &to, locked, &ring));
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
        let res = in_folder(path).and_then(|()| write_durable(path, bytes));
        self.write_started_ms.store(0, Ordering::Relaxed);
        let ms = t0.elapsed().as_millis() as u64;
        match res {
            Ok(()) => {
                self.write_failed.store(false, Ordering::Relaxed);
                {
                    let mut g = lock(&self.status);
                    g.writes_ok += 1;
                    g.last_write_ms = Some(ms);
                    g.max_write_ms = g.max_write_ms.max(ms);
                    if matches!(g.state, SdState::Error | SdState::Unknown | SdState::Failing) {
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
                self.write_failed.store(true, Ordering::Relaxed);
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
                // When RAM fails too, the copy in `pending` keeps the recording playable. A
                // recording deleted or evicted while its write waited leaves no RAM copy behind.
                if ram_ok {
                    match lock(ring).iter_mut().find(|r| r.id == id) {
                        Some(r) => {
                            r.path = ram;
                            r.store = Store::Ram;
                            r.pending = None;
                        }
                        None => {
                            let _ = std::fs::remove_file(&ram);
                        }
                    }
                }
                // A card that went away or read-only shows at once; one that is fine says
                // `failing` until a write lands.
                self.probe(false);
            }
        }
    }

    /// Move recording `id`'s file to `to` and update its entry.
    fn relocate(&self, id: u64, to: &Path, locked: bool, ring: &Ring) -> Result<(), String> {
        let from = lock(ring).iter().find(|r| r.id == id && r.store == Store::Sd).map(|r| r.path.clone());
        let Some(from) = from else { return Err(format!("recording {id} is not on the card")) };
        if from != to {
            in_folder(to).and_then(|()| std::fs::rename(&from, to)).map_err(|e| format!("move {} to {}: {e}", from.display(), to.display()))?;
            // The folder it left, when that was its last file.
            if let Some(f) = from.parent() {
                if !lock(ring).iter().any(|r| r.id != id && r.store == Store::Sd && r.path.parent() == Some(f)) {
                    let _ = std::fs::remove_dir(f);
                }
            }
        }
        if let Some(r) = lock(ring).iter_mut().find(|r| r.id == id && r.store == Store::Sd) {
            r.path = to.to_path_buf();
            r.locked = locked;
        }
        Ok(())
    }

    fn probe(&self, initial: bool) {
        let (mut state, mut detail, total, free) = probe_sd(&self.cfg);
        if initial && state == SdState::Ok {
            remove_stale_parts(&self.cfg.sd_dir);
        }
        let mut g = lock(&self.status);
        if state == SdState::Ok && self.write_failed.load(Ordering::Relaxed) {
            state = SdState::Failing;
            detail = g.last_error.clone();
        }
        g.state = state;
        g.detail = detail;
        g.total_bytes = total;
        g.free_bytes = free;
        g.last_probe_unix_ms = Some(unix_ms());
    }
}

/// Make `path`'s folder.
fn in_folder(path: &Path) -> std::io::Result<()> {
    match path.parent() {
        Some(p) => std::fs::create_dir_all(p),
        None => Ok(()),
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
pub fn index(cfg: &StorageConfig) -> Result<(Vec<Recording>, String), String> {
    if let Some(m) = cfg.sd_mount.as_deref() {
        if mount_state(m).is_none() {
            return Err(format!("{} not mounted", m.display()));
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

/// Remove the `.part` files of writes a crash or power cut interrupted, in the day folders.
fn remove_stale_parts(dir: &Path) {
    let day_folders = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| index::is_day_folder(&e.file_name().to_string_lossy()))
            .map(|e| e.path())
            .collect()
    };
    let mut folders = day_folders(dir);
    folders.extend(day_folders(&dir.join(index::LOCKED)));
    for f in folders {
        for e in std::fs::read_dir(&f).into_iter().flatten().flatten() {
            let n = e.file_name();
            let n = n.to_string_lossy();
            if n.starts_with('.') && n.ends_with(".part") {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

const DAY_MS: u64 = 86_400_000;

/// Positions (ascending) of the recordings beyond each store's retention at `now_ms`, the newest
/// recording of each store always kept (the ring is in time order). On the card: one older than
/// its days (a locked one's days when locked), when the clock and its start are both valid (the
/// board's clock starts in 1970); then, past the size, every older unlocked one (locked ones count
/// towards the size, but stay). In RAM: past the count.
pub fn evictions(ring: &VecDeque<Recording>, r: &Retention, now_ms: u64) -> Vec<usize> {
    let aged = |e: &Recording| {
        let days = if e.locked { r.locked_keep_days } else { r.sd_keep_days };
        now_ms >= MIN_VALID_MS && e.started_unix_ms >= MIN_VALID_MS && now_ms.saturating_sub(e.started_unix_ms) > u64::from(days) * DAY_MS
    };
    let (mut ram_n, mut sd_n, mut sd_bytes, mut over) = (0usize, 0usize, 0u64, false);
    let mut out = Vec::new();
    for (i, e) in ring.iter().enumerate().rev() {
        match e.store {
            Store::Sd => {
                sd_n += 1;
                if sd_n > 1 && aged(e) {
                    out.push(i);
                    continue;
                }
                if sd_n > 1 && !e.locked && (over || sd_bytes.saturating_add(e.bytes) > r.sd_max_bytes) {
                    over = true;
                    out.push(i);
                    continue;
                }
                sd_bytes = sd_bytes.saturating_add(e.bytes);
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

/// (count, bytes) of the card's locked recordings.
pub fn locked_usage(ring: &VecDeque<Recording>) -> (usize, u64) {
    ring.iter().filter(|e| e.locked && e.store == Store::Sd).fold((0, 0), |(n, b), e| (n + 1, b + e.bytes))
}

/// The folder a new card recording started at `started_unix_ms` goes to: the first of its day's
/// (in the locked tree when `locked`) holding fewer than `max` of the listed recordings.
pub fn folder_for(ring: &VecDeque<Recording>, dir: &Path, started_unix_ms: u64, locked: bool, max: usize) -> PathBuf {
    let mut n = 1;
    loop {
        let folder = index::day_folder(dir, started_unix_ms, locked, n);
        let held = ring.iter().filter(|e| e.store == Store::Sd && e.path.parent() == Some(folder.as_path())).count();
        if held < max.max(1) {
            return folder;
        }
        n += 1;
    }
}

/// Drop the recordings beyond the retention at `now_ms` and delete their files, and a card folder
/// they leave empty. Returns their names.
pub fn apply_retention(ring: &mut VecDeque<Recording>, retention: &Retention, storage: &Storage, now_ms: u64) -> Vec<String> {
    let idx = evictions(ring, retention, now_ms);
    if idx.is_empty() {
        return Vec::new();
    }
    let gone_set: std::collections::HashSet<usize> = idx.iter().copied().collect();
    // The card folders still holding a recording once these are gone.
    let in_use: std::collections::HashSet<PathBuf> = ring
        .iter()
        .enumerate()
        .filter(|(i, e)| e.store == Store::Sd && !gone_set.contains(i))
        .filter_map(|(_, e)| e.path.parent().map(Path::to_path_buf))
        .collect();
    // Removed newest first, so the oldest of a folder's files is its last delete: the folder goes
    // with that one.
    let mut oldest: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
    for &i in &idx {
        if let Some(f) = ring.get(i).filter(|e| e.store == Store::Sd).and_then(|e| e.path.parent()) {
            oldest.entry(f.to_path_buf()).or_insert(i);
        }
    }
    let mut gone = Vec::with_capacity(idx.len());
    for &i in idx.iter().rev() {
        if let Some(old) = ring.remove(i) {
            let last = old.path.parent().is_some_and(|f| !in_use.contains(f) && oldest.get(f) == Some(&i));
            storage.remove(&old, last);
            gone.push(old.file);
        }
    }
    gone
}
