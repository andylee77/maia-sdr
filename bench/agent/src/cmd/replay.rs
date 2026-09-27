//! `replay stream|check|verify`: single-pass IQ replay from staged files
//! (SD card or RAM) through a large RAM ring into `iio_writedev`.
//!
//! ```text
//! setsid sh -c 'fbench-agent replay stream --playlist P --status S --report R \
//!     | iio_writedev -u local: -b 262144 cf-ad9361-dds-core-lpc voltage0 voltage1'
//! ```
//!
//! A reader thread copies the playlist's byte ranges (in their storage format:
//! `cs16`, `cs12` = 12-bit I/Q packed in 3 bytes, or `cs8`) into the ring; the
//! main thread waits for the prefill, then converts to interleaved int16 with
//! the item's gain and writes stdout (or `--out`). `iio_writedev` pulls at the
//! DAC rate, so the ring fill is the SD card's lead over the air: a 192 MiB ring
//! of `cs12` at 4 MSPS holds 16 s, which a 23.6 MB/s card refills at
//! 11.6 MB/s net, so multi-second read stalls cost nothing. When the ring runs
//! dry anyway (`underruns`), `--on-underrun wait` (default) keeps every sample
//! and lets the timeline slip by the part of the stall the downstream buffers
//! (pipe + iio blocks) could not cover; `zero` writes zeros after
//! `--zero-after-ms` and skips the same number of source samples afterwards, so
//! sample k still airs at t0 + k / rate.
//!
//! stdout carries samples, so the final JSON goes to stderr and `--report`;
//! `--status` is rewritten every `--status-ms` for the host to poll.

use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code, Context};
use crate::safety;
use crate::sha256;
use crate::util::{self, round1, round3};
use serde_json::{json, Value};
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const STATUS_DEFAULT: &str = "/tmp/fbench_relay/status.json";

// ── formats ────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fmt {
    Cs16,
    Cs12,
    Cs8,
}

impl Fmt {
    pub fn parse(s: &str) -> AResult<Fmt> {
        match s {
            "cs16" => Ok(Fmt::Cs16),
            "cs12" => Ok(Fmt::Cs12),
            "cs8" => Ok(Fmt::Cs8),
            o => Err(AgentError::new(Code::Usage, format!("unknown sample format '{o}' (cs16|cs12|cs8)"))),
        }
    }

    /// Bytes per complex sample.
    pub fn bps(self) -> usize {
        match self {
            Fmt::Cs16 => 4,
            Fmt::Cs12 => 3,
            Fmt::Cs8 => 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Fmt::Cs16 => "cs16",
            Fmt::Cs12 => "cs12",
            Fmt::Cs8 => "cs8",
        }
    }
}

#[inline(always)]
fn sat16(v: i64) -> [u8; 2] {
    (v.clamp(-32768, 32767) as i16).to_le_bytes()
}

/// Appends interleaved little-endian int16 I/Q for `src` (whole samples).
/// Integer math with the gain in Q12 (1/4096 steps): no float, no per-sample
/// allocation, so the Cortex-A9 converts several times faster than the DAC.
pub fn convert(fmt: Fmt, src: &[u8], gain: f32, out: &mut Vec<u8>) {
    let n = src.len() / fmt.bps();
    let start = out.len();
    if fmt == Fmt::Cs16 && gain == 1.0 {
        out.extend_from_slice(&src[..n * 4]);
        return;
    }
    out.resize(start + n * 4, 0);
    let dst = &mut out[start..];
    let g = (gain as f64 * 4096.0).round() as i64;
    let sc = |x: i64| sat16((x * g + 2048) >> 12);
    match fmt {
        Fmt::Cs16 => {
            for (d, c) in dst.chunks_exact_mut(2).zip(src[..n * 4].chunks_exact(2)) {
                d.copy_from_slice(&sc(i16::from_le_bytes([c[0], c[1]]) as i64));
            }
        }
        Fmt::Cs12 => {
            for (d, c) in dst.chunks_exact_mut(4).zip(src[..n * 3].chunks_exact(3)) {
                let w = c[0] as u32 | (c[1] as u32) << 8 | (c[2] as u32) << 16;
                d[..2].copy_from_slice(&sc(util::sext12(w & 0xFFF) as i64));
                d[2..].copy_from_slice(&sc(util::sext12(w >> 12) as i64));
            }
        }
        Fmt::Cs8 => {
            for (d, c) in dst.chunks_exact_mut(4).zip(src[..n * 2].chunks_exact(2)) {
                d[..2].copy_from_slice(&sc(c[0] as i8 as i64));
                d[2..].copy_from_slice(&sc(c[1] as i8 as i64));
            }
        }
    }
}

// ── playlist ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum Item {
    File { path: String, offset: u64, length: u64, gain: f32 },
    Zeros { samples: u64 },
}

#[derive(Clone, Debug)]
pub struct Playlist {
    pub fmt: Fmt,
    pub rate_hz: f64,
    pub loops: u32,
    pub items: Vec<Item>,
}

impl Playlist {
    /// Parses `{"format", "rate_hz", "gain"?, "loops"?, "items": [{"path",
    /// "offset"?, "length"? (0 = to EOF), "gain"?} | {"zeros": samples}]}`.
    pub fn parse(v: &Value) -> AResult<Playlist> {
        let fmt = Fmt::parse(v["format"].as_str().unwrap_or("cs16"))?;
        let rate_hz = v["rate_hz"].as_f64().unwrap_or(0.0);
        let gain = v["gain"].as_f64().unwrap_or(1.0) as f32;
        let loops = v["loops"].as_u64().unwrap_or(1).max(1) as u32;
        let arr = v["items"]
            .as_array()
            .ok_or_else(|| AgentError::new(Code::Usage, "playlist needs an \"items\" array"))?;
        let mut items = Vec::new();
        for (k, it) in arr.iter().enumerate() {
            if let Some(z) = it.get("zeros") {
                let samples = z
                    .as_u64()
                    .ok_or_else(|| AgentError::new(Code::Usage, format!("item {k}: zeros must be a sample count")))?;
                items.push(Item::Zeros { samples });
                continue;
            }
            let path = it["path"]
                .as_str()
                .ok_or_else(|| AgentError::new(Code::Usage, format!("item {k}: needs \"path\" or \"zeros\"")))?
                .to_string();
            let offset = it["offset"].as_u64().unwrap_or(0);
            let length = it["length"].as_u64().unwrap_or(0);
            let bps = fmt.bps() as u64;
            if offset % bps != 0 || length % bps != 0 {
                return Err(AgentError::new(
                    Code::Usage,
                    format!("item {k}: offset/length must be multiples of {bps} bytes ({})", fmt.name()),
                ));
            }
            let g = it["gain"].as_f64().map(|g| g as f32).unwrap_or(gain);
            items.push(Item::File { path, offset, length, gain: g });
        }
        if items.is_empty() {
            return Err(AgentError::new(Code::Usage, "playlist has no items"));
        }
        Ok(Playlist { fmt, rate_hz, loops, items })
    }

    /// Resolves `length: 0` to the rest of the file and checks every range.
    pub fn resolve(&mut self) -> AResult<Value> {
        let bps = self.fmt.bps() as u64;
        let mut missing = Vec::new();
        let mut bytes = 0u64;
        for it in self.items.iter_mut() {
            match it {
                Item::File { path, offset, length, .. } => {
                    let size = match std::fs::metadata(&*path) {
                        Ok(m) => m.len(),
                        Err(_) => {
                            missing.push(path.clone());
                            continue;
                        }
                    };
                    if *length == 0 {
                        *length = size.saturating_sub(*offset) / bps * bps;
                    }
                    if *offset + *length > size {
                        return Err(AgentError::new(
                            Code::Precondition,
                            format!("{path}: range {}+{} beyond the file ({size} B): truncated upload?", offset, length),
                        ));
                    }
                    bytes += *length;
                }
                Item::Zeros { samples } => bytes += *samples * bps,
            }
        }
        if !missing.is_empty() {
            return Err(AgentError::new(Code::NotFound, format!("playlist files missing: {missing:?}"))
                .with_detail(json!({"missing": missing})));
        }
        let samples = bytes / bps * self.loops as u64;
        Ok(json!({
            "format": self.fmt.name(),
            "items": self.items.len(),
            "loops": self.loops,
            "bytes": bytes * self.loops as u64,
            "samples": samples,
            "seconds": if self.rate_hz > 0.0 { Some(round3(samples as f64 / self.rate_hz)) } else { None },
            "rate_hz": self.rate_hz,
        }))
    }
}

fn load_playlist(args: &Args) -> AResult<Playlist> {
    let p = args.req("playlist")?;
    let text = std::fs::read_to_string(&p).ctx(format!("read playlist {p}"))?;
    let v: Value = serde_json::from_str(&text)
        .map_err(|e| AgentError::new(Code::Usage, format!("playlist {p}: {e}")))?;
    Playlist::parse(&v)
}

// ── SPSC ring ──────────────────────────────────────────────────────────

pub struct Ring {
    buf: UnsafeCell<Box<[u8]>>,
    cap: u64,
    head: AtomicU64,
    tail: AtomicU64,
    done: AtomicBool,
}

// SAFETY: one producer writes only [head, tail + cap) and publishes with a
// Release store of `head`; one consumer reads only [tail, head) after an
// Acquire load and releases with `tail`. The two regions never overlap.
unsafe impl Sync for Ring {}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        let mut v = vec![0u8; cap];
        // Fault every page in now: a short-memory failure shows at start, not mid-stream.
        for i in (0..cap).step_by(4096) {
            v[i] = 1;
        }
        Ring {
            buf: UnsafeCell::new(v.into_boxed_slice()),
            cap: cap as u64,
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            done: AtomicBool::new(false),
        }
    }

    pub fn avail(&self) -> u64 {
        self.head.load(Ordering::Acquire) - self.tail.load(Ordering::Acquire)
    }

    pub fn free(&self) -> u64 {
        self.cap - self.avail()
    }

    /// Producer: appends `data` (caller checked `free() >= data.len()`).
    pub fn push(&self, data: &[u8]) {
        let head = self.head.load(Ordering::Relaxed);
        let start = (head % self.cap) as usize;
        let n = data.len();
        let cap = self.cap as usize;
        // SAFETY: see `unsafe impl Sync`; [head, head + n) is free.
        let buf = unsafe { &mut *self.buf.get() };
        let first = n.min(cap - start);
        buf[start..start + first].copy_from_slice(&data[..first]);
        if first < n {
            buf[..n - first].copy_from_slice(&data[first..]);
        }
        self.head.store(head + n as u64, Ordering::Release);
    }

    /// Consumer: copies `dst.len()` bytes from the tail (caller checked `avail`).
    pub fn peek(&self, dst: &mut [u8]) {
        let tail = self.tail.load(Ordering::Relaxed);
        let start = (tail % self.cap) as usize;
        let n = dst.len();
        let cap = self.cap as usize;
        // SAFETY: see `unsafe impl Sync`; [tail, tail + n) is published.
        let buf = unsafe { &*self.buf.get() };
        let first = n.min(cap - start);
        dst[..first].copy_from_slice(&buf[start..start + first]);
        if first < n {
            dst[first..].copy_from_slice(&buf[..n - first]);
        }
    }

    pub fn consume(&self, n: u64) {
        let t = self.tail.load(Ordering::Relaxed);
        self.tail.store(t + n, Ordering::Release);
    }

    pub fn head(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    pub fn tail(&self) -> u64 {
        self.tail.load(Ordering::Acquire)
    }
}

// ── shared state ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Seg {
    // Absolute ring byte where this item starts.
    start: u64,
    // Playlist item index (loops continue the count).
    index: usize,
    gain: f32,
}

#[derive(Default)]
struct ReadStats {
    bytes: AtomicU64,
    calls: AtomicU64,
    busy_us: AtomicU64,
    max_us: AtomicU64,
    slow: AtomicU64,
    wait_full_us: AtomicU64,
    item: AtomicU64,
    // Log2 histogram of read-call latency in ms (bucket k: [2^k, 2^(k+1)) ms, bucket 0 < 2 ms).
    hist: [AtomicU64; 16],
    stalls: Mutex<Vec<Value>>,
    error: Mutex<Option<String>>,
}

struct Shared {
    ring: Ring,
    segs: Mutex<VecDeque<Seg>>,
    rs: ReadStats,
}

struct ReaderCfg {
    chunk: usize,
    stall_ms: u64,
    inject: Vec<(u64, u64)>,
}

fn reader(pl: Playlist, sh: Arc<Shared>, cfg: ReaderCfg) {
    let r = reader_inner(&pl, &sh, &cfg);
    if let Err(e) = r {
        *sh.rs.error.lock().unwrap() = Some(e.msg);
    }
    sh.ring.done.store(true, Ordering::Release);
}

fn wait_free(sh: &Shared, n: u64) -> bool {
    let t = Instant::now();
    while sh.ring.free() < n {
        if safety::stop_requested() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    sh.rs.wait_full_us.fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
    true
}

fn reader_inner(pl: &Playlist, sh: &Shared, cfg: &ReaderCfg) -> AResult<()> {
    let bps = pl.fmt.bps();
    let chunk = (cfg.chunk / bps).max(1) * bps;
    let mut tmp = vec![0u8; chunk];
    let zeros = vec![0u8; chunk];
    let mut pos_in = 0u64; // input bytes pushed (for --inject-stall)
    let mut inject: VecDeque<(u64, u64)> = cfg.inject.iter().copied().collect();
    let mut index = 0usize;
    for _ in 0..pl.loops {
        for it in &pl.items {
            if safety::stop_requested() {
                return Ok(());
            }
            let gain = match it {
                Item::File { gain, .. } => *gain,
                Item::Zeros { .. } => 1.0,
            };
            sh.segs.lock().unwrap().push_back(Seg { start: sh.ring.head(), index, gain });
            sh.rs.item.store(index as u64, Ordering::Relaxed);
            index += 1;
            match it {
                Item::Zeros { samples } => {
                    let mut left = samples * bps as u64;
                    while left > 0 {
                        let n = left.min(chunk as u64);
                        if !wait_free(sh, n) {
                            return Ok(());
                        }
                        sh.ring.push(&zeros[..n as usize]);
                        left -= n;
                    }
                }
                Item::File { path, offset, length, .. } => {
                    let mut f = std::fs::File::open(path).ctx(format!("open {path}"))?;
                    f.seek(SeekFrom::Start(*offset))?;
                    advise_sequential(&f);
                    let mut left = *length;
                    let mut done_since_drop = 0u64;
                    let mut file_pos = *offset;
                    while left > 0 {
                        let n = left.min(chunk as u64) as usize;
                        if !wait_free(sh, n as u64) {
                            return Ok(());
                        }
                        let t = Instant::now();
                        if let Some(&(at, ms)) = inject.front() {
                            if pos_in >= at {
                                // Test hook: an SD stall inside this read call.
                                inject.pop_front();
                                std::thread::sleep(Duration::from_millis(ms));
                            }
                        }
                        let mut got = 0usize;
                        while got < n {
                            let r = f.read(&mut tmp[got..n])?;
                            if r == 0 {
                                return Err(AgentError::new(
                                    Code::Error,
                                    format!("{path}: unexpected EOF at {} (file shorter than the playlist range)", file_pos + got as u64),
                                ));
                            }
                            got += r;
                        }
                        let us = t.elapsed().as_micros() as u64;
                        note_read(sh, us, pos_in, cfg.stall_ms);
                        sh.ring.push(&tmp[..n]);
                        sh.rs.bytes.fetch_add(n as u64, Ordering::Relaxed);
                        pos_in += n as u64;
                        file_pos += n as u64;
                        left -= n as u64;
                        done_since_drop += n as u64;
                        if done_since_drop >= 32 << 20 {
                            drop_cache(&f, file_pos - done_since_drop, done_since_drop);
                            done_since_drop = 0;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn note_read(sh: &Shared, us: u64, pos: u64, stall_ms: u64) {
    let rs = &sh.rs;
    rs.calls.fetch_add(1, Ordering::Relaxed);
    rs.busy_us.fetch_add(us, Ordering::Relaxed);
    rs.max_us.fetch_max(us, Ordering::Relaxed);
    let ms = us / 1000;
    let b = if ms < 2 { 0 } else { (63 - ms.leading_zeros() as usize).min(15) };
    rs.hist[b].fetch_add(1, Ordering::Relaxed);
    if ms >= 200 {
        rs.slow.fetch_add(1, Ordering::Relaxed);
    }
    if ms >= stall_ms {
        let mut s = rs.stalls.lock().unwrap();
        if s.len() < 200 {
            s.push(json!({"input_byte": pos, "ms": ms, "ring_fill_bytes": sh.ring.avail(),
                          "t_unix": round3(util::unix_now_f64())}));
        }
    }
}

#[cfg(target_os = "linux")]
fn advise_sequential(f: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
    }
}

#[cfg(not(target_os = "linux"))]
fn advise_sequential(_f: &std::fs::File) {}

/// Drops consumed file pages so a multi-GB pass does not churn the page cache.
#[cfg(target_os = "linux")]
fn drop_cache(f: &std::fs::File, off: u64, len: u64) {
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), off as libc::off_t, len as libc::off_t, libc::POSIX_FADV_DONTNEED);
    }
}

#[cfg(not(target_os = "linux"))]
fn drop_cache(_f: &std::fs::File, _off: u64, _len: u64) {}

// ── output ─────────────────────────────────────────────────────────────

fn open_out(path: Option<&str>) -> AResult<Box<dyn Write>> {
    if let Some(p) = path {
        let p = util::check_write_path(p)?;
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        return Ok(Box::new(std::fs::File::create(&p).ctx(format!("create {}", p.display()))?));
    }
    util::set_stdout_is_data(true);
    raw_stdout()
}

/// stdout as an unbuffered file: std's stdout is line buffered (it would
/// flush at every 0x0A byte of sample data).
#[cfg(unix)]
fn raw_stdout() -> AResult<Box<dyn Write>> {
    use std::os::unix::io::FromRawFd;
    #[cfg(target_os = "linux")]
    unsafe {
        // Larger pipe: fewer wake-ups between the relay and iio_writedev (best effort).
        libc::fcntl(1, libc::F_SETPIPE_SZ, 1 << 20);
    }
    // SAFETY: fd 1 stays open for the life of the process; ManuallyDrop keeps
    // it open when the writer is dropped.
    let f = unsafe { std::fs::File::from_raw_fd(1) };
    Ok(Box::new(StdoutFile(std::mem::ManuallyDrop::new(f))))
}

#[cfg(unix)]
struct StdoutFile(std::mem::ManuallyDrop<std::fs::File>);

#[cfg(unix)]
impl Write for StdoutFile {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(not(unix))]
fn raw_stdout() -> AResult<Box<dyn Write>> {
    Ok(Box::new(std::io::stdout()))
}

// ── stream ─────────────────────────────────────────────────────────────

fn parse_inject(s: Option<String>) -> AResult<Vec<(u64, u64)>> {
    let mut v = Vec::new();
    if let Some(s) = s {
        for part in s.split(',').filter(|p| !p.is_empty()) {
            let (a, b) = part
                .split_once(':')
                .ok_or_else(|| AgentError::new(Code::Usage, "--inject-stall wants BYTE:MS[,BYTE:MS]"))?;
            let at = util::parse_size(a).ok_or_else(|| AgentError::new(Code::Usage, format!("bad byte offset '{a}'")))?;
            let ms = b.parse().map_err(|_| AgentError::new(Code::Usage, format!("bad ms '{b}'")))?;
            v.push((at, ms));
        }
    }
    v.sort();
    Ok(v)
}

struct Writer {
    samples_out: u64,
    bytes_out: u64,
    underruns: u64,
    underrun_ms: f64,
    zero_samples: u64,
    skipped_samples: u64,
    events: Vec<Value>,
    min_fill: Option<u64>,
    item: usize,
    t_first_unix: Option<f64>,
    t_first: Option<Instant>,
    prefill_s: f64,
    state: &'static str,
}

fn status_json(w: &Writer, sh: &Shared, pl_info: &Value, rate: f64, bps: usize, started: Instant) -> Value {
    let rs = &sh.rs;
    let bytes_in = rs.bytes.load(Ordering::Relaxed);
    let busy = rs.busy_us.load(Ordering::Relaxed) as f64 / 1e6;
    let hist: Vec<u64> = rs.hist.iter().map(|h| h.load(Ordering::Relaxed)).collect();
    let fill = sh.ring.avail();
    let elapsed = w.t_first.map(|t| t.elapsed().as_secs_f64());
    json!({
        "state": w.state,
        "now_unix": round3(util::unix_now_f64()),
        "started_s_ago": round3(started.elapsed().as_secs_f64()),
        "t_first_out_unix": w.t_first_unix.map(round3),
        "prefill_s": round3(w.prefill_s),
        "item": w.item,
        "reader_item": rs.item.load(Ordering::Relaxed),
        "samples_out": w.samples_out,
        "seconds_out": if rate > 0.0 { Some(round3(w.samples_out as f64 / rate)) } else { None },
        "stream_elapsed_s": elapsed.map(round3),
        "bytes_out": w.bytes_out,
        "bytes_in": bytes_in,
        "total_bytes_in": pl_info["bytes"],
        "ring_bytes": sh.ring.cap,
        "ring_fill_bytes": fill,
        "ring_fill_s": if rate > 0.0 { Some(round3(fill as f64 / bps as f64 / rate)) } else { None },
        "ring_min_fill_bytes": w.min_fill,
        "ring_min_fill_s": match (w.min_fill, rate > 0.0) {
            (Some(m), true) => Some(round3(m as f64 / bps as f64 / rate)),
            _ => None,
        },
        "read_mbs": if busy > 0.0 { Some(round1(bytes_in as f64 / 1e6 / busy)) } else { None },
        "read_calls": rs.calls.load(Ordering::Relaxed),
        "read_max_ms": round1(rs.max_us.load(Ordering::Relaxed) as f64 / 1000.0),
        "read_slow_200ms": rs.slow.load(Ordering::Relaxed),
        "read_hist_log2_ms": hist,
        "read_stalls": rs.stalls.lock().unwrap().clone(),
        "reader_wait_full_s": round3(rs.wait_full_us.load(Ordering::Relaxed) as f64 / 1e6),
        "underruns": w.underruns,
        "underrun_ms": round1(w.underrun_ms),
        "underrun_events": w.events,
        "zero_samples": w.zero_samples,
        "skipped_samples": w.skipped_samples,
        "reader_error": rs.error.lock().unwrap().clone(),
    })
}

/// Test hook (`--pace-hz`): hold the output to a DAC-like rate when writing
/// to a file, where nothing downstream applies back-pressure.
fn pace(pace_hz: Option<f64>, w: &Writer) {
    if let (Some(hz), Some(t0)) = (pace_hz, w.t_first) {
        let due = w.samples_out as f64 / hz;
        let el = t0.elapsed().as_secs_f64();
        if due > el {
            std::thread::sleep(Duration::from_secs_f64(due - el));
        }
    }
}

fn write_status(path: &Option<std::path::PathBuf>, v: &Value) {
    if let Some(p) = path {
        let _ = util::write_file(p, v.to_string().as_bytes());
    }
}

fn stream(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let mut pl = load_playlist(args)?;
    let ring_mb = args.u64_or("ring-mb", 192)?.clamp(1, 768);
    let prefill_mb = args.u64_opt("prefill-mb")?;
    let chunk_kb = args.u64_or("chunk-kb", 1024)?.clamp(4, 16384);
    let status_ms = args.u64_or("status-ms", 1000)?.max(100);
    let stall_ms = args.u64_or("stall-ms", 500)?;
    let mode = args.opt_or("on-underrun", "wait")?;
    let zero_after_ms = args.u64_or("zero-after-ms", 250)?;
    let pace_hz = args.f64_opt("pace-hz")?;
    let out_path = args.opt("out")?;
    let status = args.opt("status")?.map(|s| util::check_write_path(s)).transpose()?;
    let report = args.opt("report")?.map(|s| util::check_write_path(s)).transpose()?;
    let inject = parse_inject(args.opt("inject-stall")?)?;
    let rate_opt = args.f64_opt("rate-hz")?;
    args.finish()?;
    if mode != "wait" && mode != "zero" {
        return Err(AgentError::new(Code::Usage, "--on-underrun must be wait or zero"));
    }
    if let Some(r) = rate_opt {
        pl.rate_hz = r;
    }
    let info = pl.resolve()?;
    let fmt = pl.fmt;
    let bps = fmt.bps();
    let rate = pl.rate_hz;
    let cap = ((ring_mb << 20) as usize / bps) * bps;
    let chunk = ((chunk_kb as usize) << 10) / bps * bps;
    let total_in = info["bytes"].as_u64().unwrap_or(0);
    let prefill = match prefill_mb {
        Some(mb) => ((mb << 20) as u64).min(cap as u64),
        None => (cap - chunk) as u64,
    }
    .min(total_in);
    let started = Instant::now();
    let sh = Arc::new(Shared { ring: Ring::new(cap), segs: Mutex::new(VecDeque::new()), rs: ReadStats::default() });
    let mut out = open_out(out_path.as_deref())?;
    ctx.log(format!("replay: {} items, {} B in, ring {cap} B, prefill {prefill} B", pl.items.len(), total_in));
    let rcfg = ReaderCfg { chunk, stall_ms, inject };
    let pl2 = pl.clone();
    let sh2 = Arc::clone(&sh);
    let rd = std::thread::Builder::new()
        .name("replay-reader".into())
        .spawn(move || reader(pl2, sh2, rcfg))
        .map_err(|e| AgentError::new(Code::Error, format!("spawn reader: {e}")))?;

    let mut w = Writer {
        samples_out: 0,
        bytes_out: 0,
        underruns: 0,
        underrun_ms: 0.0,
        zero_samples: 0,
        skipped_samples: 0,
        events: Vec::new(),
        min_fill: None,
        item: 0,
        t_first_unix: None,
        t_first: None,
        prefill_s: 0.0,
        state: "prefill",
    };
    let mut last_status = Instant::now() - Duration::from_secs(10);
    let tick = |w: &Writer, last: &mut Instant| {
        if last.elapsed() >= Duration::from_millis(status_ms) {
            write_status(&status, &status_json(w, &sh, &info, rate, bps, started));
            *last = Instant::now();
        }
    };
    // Prefill: the ring's lead over the air absorbs later SD stalls.
    while sh.ring.avail() < prefill && !sh.ring.done.load(Ordering::Acquire) && !safety::stop_requested() {
        tick(&w, &mut last_status);
        std::thread::sleep(Duration::from_millis(5));
    }
    w.prefill_s = started.elapsed().as_secs_f64();
    w.state = "streaming";
    let mut inbuf = vec![0u8; chunk];
    let mut outbuf: Vec<u8> = Vec::with_capacity(chunk / bps * 4 + 16);
    let zero_block = vec![0u8; ((rate / 100.0).max(256.0) as usize) * 4];
    let mut debt = 0u64; // source samples to skip (zero mode)
    let mut stopped = false;
    let mut downstream_closed = false;
    loop {
        if safety::stop_requested() {
            stopped = true;
            break;
        }
        tick(&w, &mut last_status);
        let avail = sh.ring.avail() / bps as u64 * bps as u64;
        if avail == 0 {
            if sh.ring.done.load(Ordering::Acquire) && sh.ring.avail() < bps as u64 {
                break;
            }
            if w.t_first.is_none() {
                // Nothing aired yet (prefill 0): waiting is not an underrun.
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            // Underrun: the reader fell behind the air.
            let t_u = Instant::now();
            let at = w.samples_out;
            let mut zeros = 0u64;
            w.state = "underrun";
            while sh.ring.avail() < bps as u64 && !sh.ring.done.load(Ordering::Acquire) && !safety::stop_requested() {
                if mode == "zero" && t_u.elapsed() >= Duration::from_millis(zero_after_ms) {
                    if let Err(e) = out.write_all(&zero_block) {
                        downstream_closed = e.kind() == std::io::ErrorKind::BrokenPipe;
                        if !downstream_closed {
                            return Err(e.into());
                        }
                        break;
                    }
                    let n = (zero_block.len() / 4) as u64;
                    zeros += n;
                    w.samples_out += n;
                    w.bytes_out += zero_block.len() as u64;
                    pace(pace_hz, &w);
                } else {
                    std::thread::sleep(Duration::from_millis(1));
                }
                tick(&w, &mut last_status);
            }
            if downstream_closed {
                break;
            }
            let ms = t_u.elapsed().as_secs_f64() * 1000.0;
            if sh.ring.done.load(Ordering::Acquire) && sh.ring.avail() < bps as u64 && zeros == 0 && ms < 5.0 {
                continue; // end of stream, not an underrun
            }
            w.underruns += 1;
            w.underrun_ms += ms;
            w.zero_samples += zeros;
            debt += zeros;
            if w.events.len() < 200 {
                w.events.push(json!({"at_sample": at, "at_s": if rate > 0.0 { Some(round3(at as f64 / rate)) } else { None },
                                     "ms": round1(ms), "zero_samples": zeros, "item": w.item,
                                     "t_unix": round3(util::unix_now_f64())}));
            }
            w.state = "streaming";
            continue;
        }
        if debt > 0 {
            let skip = (avail / bps as u64).min(debt);
            sh.ring.consume(skip * bps as u64);
            debt -= skip;
            w.skipped_samples += skip;
            continue;
        }
        // Current item (gain) and the bytes up to the next item boundary.
        let tail = sh.ring.tail();
        let (gain, limit) = {
            let mut segs = sh.segs.lock().unwrap();
            while segs.len() > 1 && segs[1].start <= tail {
                segs.pop_front();
            }
            let cur = segs.front().copied();
            let next = segs.get(1).map(|s| s.start);
            if let Some(c) = cur {
                w.item = c.index;
            }
            (cur.map(|c| c.gain).unwrap_or(1.0), next.map(|n| n - tail))
        };
        let mut n = avail.min(chunk as u64);
        if let Some(l) = limit {
            n = n.min(l);
        }
        let n = (n / bps as u64 * bps as u64) as usize;
        if n == 0 {
            continue;
        }
        sh.ring.peek(&mut inbuf[..n]);
        outbuf.clear();
        convert(fmt, &inbuf[..n], gain, &mut outbuf);
        if w.t_first.is_none() {
            w.t_first = Some(Instant::now());
            w.t_first_unix = Some(util::unix_now_f64());
        }
        if let Err(e) = out.write_all(&outbuf) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                downstream_closed = true;
                break;
            }
            return Err(e.into());
        }
        sh.ring.consume(n as u64);
        w.samples_out += (n / bps) as u64;
        w.bytes_out += outbuf.len() as u64;
        let fill = sh.ring.avail();
        if !sh.ring.done.load(Ordering::Acquire) {
            w.min_fill = Some(w.min_fill.map_or(fill, |m| m.min(fill)));
        }
        pace(pace_hz, &w);
    }
    let _ = out.flush();
    drop(out);
    if stopped || downstream_closed {
        // Unblock and stop the reader.
        sh.ring.done.store(true, Ordering::Release);
    }
    let reader_err = sh.rs.error.lock().unwrap().clone();
    if !stopped {
        let _ = rd.join();
    }
    w.state = if let Some(_) = &reader_err {
        "error"
    } else if stopped {
        "stopped"
    } else if downstream_closed {
        "downstream_closed"
    } else {
        "done"
    };
    let mut v = status_json(&w, &sh, &info, rate, bps, started);
    v["playlist"] = info.clone();
    v["format"] = json!(fmt.name());
    v["on_underrun"] = json!(mode);
    v["skip_debt_left"] = json!(debt);
    v["complete"] = json!(w.state == "done" && w.samples_out >= info["samples"].as_u64().unwrap_or(0));
    write_status(&status, &v);
    write_status(&report, &v);
    if let Some(e) = reader_err {
        return Err(AgentError::new(Code::Error, format!("replay reader: {e}")).with_detail(v));
    }
    Ok(v)
}

// ── check / verify ─────────────────────────────────────────────────────

fn check(args: &Args) -> AResult<Value> {
    let mut pl = load_playlist(args)?;
    args.finish()?;
    let mut v = pl.resolve()?;
    let mem = crate::sys::meminfo();
    v["mem_available_kb"] = json!(mem.get("MemAvailable").copied());
    Ok(v)
}

fn verify(args: &Args) -> AResult<Value> {
    let path = args.req("file")?;
    let offset = args.u64_or("offset", 0)?;
    let length = args.u64_or("length", 0)?;
    let want = args.opt("sha256")?;
    args.finish()?;
    let mut f = std::fs::File::open(&path).ctx(format!("open {path}"))?;
    let size = f.metadata()?.len();
    let len = if length == 0 { size.saturating_sub(offset) } else { length };
    f.seek(SeekFrom::Start(offset))?;
    let mut h = sha256::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut left = len;
    let t = Instant::now();
    let mut max_ms = 0.0f64;
    while left > 0 {
        safety::check_stop()?;
        let n = left.min(buf.len() as u64) as usize;
        let tr = Instant::now();
        f.read_exact(&mut buf[..n])?;
        max_ms = max_ms.max(tr.elapsed().as_secs_f64() * 1000.0);
        h.update(&buf[..n]);
        left -= n as u64;
    }
    let secs = t.elapsed().as_secs_f64();
    let hex = sha256::hex(&h.finish());
    let ok = want.as_deref().map(|w| w.eq_ignore_ascii_case(&hex));
    Ok(json!({"file": path, "bytes": len, "sha256": hex, "match": ok,
              "read_mbs": round1(len as f64 / 1e6 / secs.max(1e-9)), "read_max_ms": round1(max_ms)}))
}

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    match sub(args, 1, &["stream", "check", "verify"])? {
        "stream" => stream(ctx, args),
        "check" => check(args),
        _ => verify(args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_round_trip_to_int16() {
        let mut out = Vec::new();
        // cs12: I = -5, Q = 2047 -> w = 0xFFB | 0x7FF << 12
        let w: u32 = (0xFFBu32) | (0x7FFu32 << 12);
        convert(Fmt::Cs12, &[w as u8, (w >> 8) as u8, (w >> 16) as u8], 2.0, &mut out);
        assert_eq!(out, [(-10i16).to_le_bytes(), 4094i16.to_le_bytes()].concat());
        out.clear();
        convert(Fmt::Cs8, &[0x80, 0x7F], 100.0, &mut out);
        assert_eq!(out, [(-12800i16).to_le_bytes(), 12700i16.to_le_bytes()].concat());
        out.clear();
        convert(Fmt::Cs16, &30000i16.to_le_bytes().repeat(2), 2.0, &mut out);
        assert_eq!(out, [32767i16.to_le_bytes(), 32767i16.to_le_bytes()].concat(), "saturates");
        out.clear();
        convert(Fmt::Cs16, &[1, 2, 3, 4, 5], 1.0, &mut out);
        assert_eq!(out, vec![1, 2, 3, 4], "whole samples only");
    }

    #[test]
    fn ring_wraps() {
        let r = Ring::new(10);
        r.push(&[1, 2, 3, 4, 5, 6, 7]);
        let mut d = [0u8; 5];
        r.peek(&mut d);
        r.consume(5);
        assert_eq!(d, [1, 2, 3, 4, 5]);
        r.push(&[8, 9, 10, 11, 12]);
        assert_eq!(r.avail(), 7);
        let mut d = [0u8; 7];
        r.peek(&mut d);
        assert_eq!(d, [6, 7, 8, 9, 10, 11, 12]);
    }

    #[test]
    fn playlist_parse_and_alignment() {
        let v = json!({"format": "cs12", "rate_hz": 4e6, "gain": 3.5,
                       "items": [{"path": "/x", "offset": 3, "length": 6}, {"zeros": 10}]});
        let p = Playlist::parse(&v).unwrap();
        assert_eq!(p.fmt, Fmt::Cs12);
        match &p.items[0] {
            Item::File { gain, .. } => assert_eq!(*gain, 3.5),
            _ => panic!(),
        }
        let bad = json!({"format": "cs12", "items": [{"path": "/x", "offset": 2}]});
        assert!(Playlist::parse(&bad).is_err());
        assert!(Playlist::parse(&json!({"format": "cs9", "items": []})).is_err());
    }
}
