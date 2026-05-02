//! Wideband raw IQ task — drains the new 8 MSPS / 8 MHz BW IQ DMA
//! ring (2026-05-03) and optionally captures a fixed-duration window
//! to a tmpfs file for offline analysis. Stage 1 of the PS-side
//! software P25 stack: the DDC + LSM demod will hang off the same
//! drain loop in subsequent commits.
//!
//! Linux-only: depends on `fpga::IpCore`.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use tokio::sync::{mpsc, Mutex};

use crate::hardware::fpga;
use crate::lsm::Complex32;

/// Wideband IQ DMA produces 8 MSPS complex samples (i16 I + i16 Q).
const WIDEBAND_IQ_RATE_HZ: u64 = 8_000_000;
/// 4 bytes per complex sample (i16 + i16).
const SAMPLE_BYTES: u64 = 4;
/// Captures land here. Tmpfs so the SD card isn't beaten on; user
/// scp-pulls the file out for offline GNU Radio / numpy analysis.
const CAPTURE_DIR: &str = "/tmp/p25_iq_captures";

/// Shared capture-mode state. Held by the spawn task; the API side
/// modifies it to start/inspect captures.
#[derive(Default)]
pub struct WidebandIqCaptureState {
    inner: Mutex<CaptureInner>,
}

#[derive(Default)]
struct CaptureInner {
    active: Option<ActiveCapture>,
    last_path: Option<PathBuf>,
    last_bytes: u64,
    last_finished_at: Option<Instant>,
}

struct ActiveCapture {
    file: std::fs::File,
    bytes_remaining: u64,
    bytes_written: u64,
    started_at: Instant,
    path: PathBuf,
    overflow_seen: bool,
}

impl WidebandIqCaptureState {
    /// Begins a capture of the next `seconds` seconds of wideband IQ
    /// to a fresh file in /tmp/p25_iq_captures/. Cancels any
    /// in-flight capture. Returns the path so the caller can echo it
    /// back to the operator.
    pub async fn start(&self, seconds: f64) -> anyhow::Result<PathBuf> {
        if !(0.0..=30.0).contains(&seconds) {
            anyhow::bail!(
                "wideband IQ capture duration {seconds:.2} s outside \
                 [0, 30] (32 MB/s would chew through tmpfs fast)"
            );
        }
        std::fs::create_dir_all(CAPTURE_DIR).with_context(
            || format!("failed to create {CAPTURE_DIR}"))?;

        let secs_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = PathBuf::from(format!(
            "{CAPTURE_DIR}/wb_iq_{}_{:.1}s.cs16",
            secs_unix, seconds,
        ));

        let file = std::fs::File::create(&path)
            .with_context(|| format!("failed to create {path:?}"))?;

        let bytes = (seconds.max(0.0)
            * WIDEBAND_IQ_RATE_HZ as f64
            * SAMPLE_BYTES as f64) as u64;

        let mut inner = self.inner.lock().await;
        inner.active = Some(ActiveCapture {
            file,
            bytes_remaining: bytes,
            bytes_written: 0,
            started_at: Instant::now(),
            path: path.clone(),
            overflow_seen: false,
        });
        Ok(path)
    }

    pub async fn snapshot(&self) -> WidebandIqCaptureSnapshot {
        let inner = self.inner.lock().await;
        WidebandIqCaptureSnapshot {
            active: inner.active.as_ref().map(|a| ActiveCaptureSnapshot {
                path: a.path.clone(),
                bytes_written: a.bytes_written,
                bytes_remaining: a.bytes_remaining,
                seconds_elapsed: a.started_at.elapsed().as_secs_f64(),
                overflow_seen: a.overflow_seen,
            }),
            last_path: inner.last_path.clone(),
            last_bytes: inner.last_bytes,
            last_finished_at_secs_ago: inner.last_finished_at
                .map(|t| t.elapsed().as_secs_f64()),
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WidebandIqCaptureSnapshot {
    pub active: Option<ActiveCaptureSnapshot>,
    pub last_path: Option<PathBuf>,
    pub last_bytes: u64,
    pub last_finished_at_secs_ago: Option<f64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ActiveCaptureSnapshot {
    pub path: PathBuf,
    pub bytes_written: u64,
    pub bytes_remaining: u64,
    pub seconds_elapsed: f64,
    pub overflow_seen: bool,
}

/// Spawns the wideband-IQ DMA drain task. Master DMA enable is set
/// inside the task on first entry (the HDL ring writes nothing until
/// `wideband_iq_enable=1`). Capture mode is toggled at runtime via
/// `WidebandIqCaptureState::start`.
///
/// `sw_demod_tx`, when present, gets a `Vec<Complex32>` per drained
/// sub-buffer (i16 LE → f32 conversion happens here so the consumer
/// can stay pure-DSP). Channel is bounded; `try_send` failures (full
/// channel = consumer fell behind) are dropped + counted, NOT awaited
/// — this task must keep up with 32 MB/s no matter what the demod
/// downstream is doing.
pub fn spawn_wideband_iq_reader(
    waiter: fpga::InterruptWaiter,
    core: Arc<Mutex<fpga::IpCore>>,
    capture: Arc<WidebandIqCaptureState>,
    sw_demod_tx: Option<mpsc::Sender<Vec<Complex32>>>,
) {
    tokio::spawn(async move {
        use std::io::Write;

        // 2026-05-03 dual-DDC pivot: DMA stays OFF at boot. The HDL
        // chain is the production path; wideband_iq is now a
        // diagnostic-only stream. Two callers turn it on at runtime:
        //   * `POST /api/sw_demod?enabled=1` (live software A/B)
        //   * `POST /api/wideband_iq_capture` (one-shot offline dump)
        // Both flip it back off when they're done. Until then this
        // task sleeps in `waiter.wait()` consuming zero CPU.
        tracing::info!(
            target: "p25_wideband_iq",
            "wideband IQ reader spawned (DMA off; enable via \
             /api/sw_demod or /api/wideband_iq_capture)"
        );

        let mut wakeups: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut last_log = Instant::now();
        let mut sw_demod_dropped: u64 = 0;

        loop {
            waiter.wait().await;
            wakeups += 1;

            // Drain ring under lock; copy out so we can release the
            // mutex before the file write blocks. At 32 MB/s steady
            // state we rarely see more than a couple of 1 MB sub-
            // buffers per IRQ.
            let (buffers, overflow) = {
                let mut core = core.lock().await;
                let bufs = core
                    .read_wideband_iq_buffers()
                    .iter()
                    .map(|b| b.to_vec())
                    .collect::<Vec<_>>();
                let overflow = core.wideband_iq_overflow();
                (bufs, overflow)
            };

            if buffers.is_empty() {
                continue;
            }

            let wake_bytes: usize = buffers.iter().map(|b| b.len()).sum();
            total_bytes += wake_bytes as u64;

            // Software-demod tee. Reinterpret the i16 LE byte stream
            // as &[i16] (host is LE on ARMv7 so byte order matches),
            // then bulk-convert to Complex32. The earlier byte-shift
            // path was burning ~16 % of one Cortex-A9 core on
            // bounds-checked per-byte unpacking at 8 MSPS.
            // try_send must NOT block — drop on full channel.
            if let Some(tx) = sw_demod_tx.as_ref() {
                for buffer in &buffers {
                    // SAFETY: maia-kmod hands us 8-byte-aligned DMA
                    // sub-buffers and i16 LE matches host LE on ARMv7.
                    // Length is a multiple of 4 (one IQ pair = 4 B)
                    // by construction of the HDL packer (iq_packer.py).
                    let n_pairs = buffer.len() / 4;
                    let i16_buf: &[i16] = unsafe {
                        std::slice::from_raw_parts(
                            buffer.as_ptr() as *const i16,
                            n_pairs * 2,
                        )
                    };
                    let mut samples: Vec<Complex32> =
                        Vec::with_capacity(n_pairs);
                    for pair in i16_buf.chunks_exact(2) {
                        samples.push(Complex32::new(
                            pair[0] as f32,
                            pair[1] as f32,
                        ));
                    }
                    if tx.try_send(samples).is_err() {
                        sw_demod_dropped += 1;
                    }
                }
            }

            // Capture-mode tee. Single contiguous file; sync writes
            // are safe because /tmp is tmpfs (no SD wear, no fsync
            // round-trips).
            {
                let mut inner = capture.inner.lock().await;
                if let Some(active) = inner.active.as_mut() {
                    if overflow {
                        active.overflow_seen = true;
                    }
                    let mut finish = false;
                    for buffer in &buffers {
                        if active.bytes_remaining == 0 {
                            finish = true;
                            break;
                        }
                        let take = std::cmp::min(
                            active.bytes_remaining as usize,
                            buffer.len(),
                        );
                        if let Err(e) = active.file.write_all(&buffer[..take]) {
                            tracing::error!(
                                target: "p25_wideband_iq",
                                "capture file write failed: {e}; \
                                 aborting capture"
                            );
                            finish = true;
                            break;
                        }
                        active.bytes_written += take as u64;
                        active.bytes_remaining -= take as u64;
                        if active.bytes_remaining == 0 {
                            finish = true;
                        }
                    }
                    if finish {
                        if let Some(done) = inner.active.take() {
                            let _ = done.file.sync_all();
                            tracing::info!(
                                target: "p25_wideband_iq",
                                "capture finished: {} bytes -> {:?} ({:.2} s elapsed, overflow={})",
                                done.bytes_written,
                                done.path,
                                done.started_at.elapsed().as_secs_f64(),
                                done.overflow_seen,
                            );
                            inner.last_path = Some(done.path);
                            inner.last_bytes = done.bytes_written;
                            inner.last_finished_at = Some(Instant::now());
                        }
                    }
                }
            }

            if last_log.elapsed().as_secs_f64() >= 5.0 {
                let elapsed = last_log.elapsed().as_secs_f64();
                let mb_s = (total_bytes as f64) / 1.0e6 / elapsed;
                let overflow_str = if overflow { " OVERFLOW" } else { "" };
                tracing::info!(
                    target: "p25_wideband_iq",
                    "wideband IQ throughput: {wakeups} wakeups, \
                     {} MiB in {:.1} s ({:.1} MB/s){overflow_str} \
                     sw_demod_dropped={sw_demod_dropped}",
                    total_bytes / (1024 * 1024), elapsed, mb_s,
                );
                last_log = Instant::now();
                total_bytes = 0;
            }
        }
    });
}
