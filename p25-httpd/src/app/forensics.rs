//! On-device forensics capture for HDL-vs-SW dibit diff (Track 2,
//! 2026-05-03).
//!
//! Why: host-side polling of `/api/traffic_dibit_capture` (rolling
//! 2048-dibit ring = 426 ms at 4800 sym/s) loses data when any single
//! HTTP poll lags > 426 ms. Track 2 needs a contiguous dibit stream
//! aligned to a wideband IQ capture so we can diff HDL chain output
//! against the SW oracle. Solution: large RAM ring populated directly
//! by the traffic dibit reader, automatically armed on CallOpen and
//! finalised on CallClose, written to /tmp for scp-down.
//!
//! Lifecycle (when `armed=true`):
//!
//! ```text
//!   CallOpen   -> set active=true, allocate fresh run dir under
//!                 /tmp/p25_forensics/run_<unix>_tg<TG>_<freq>/,
//!                 trigger wideband_iq_capture in parallel
//!   reader tee -> every 64-bit DMA word (32 dibits) appends to
//!                 dibit_buf while active=true (lock-free check)
//!   CallClose  -> set active=false, wait <=N s for wideband to
//!                 drain, write all artefacts, optionally re-arm
//! ```
//!
//! Output format per run (matches host-side
//! `tools/p25_chain_forensics_capture.py`):
//!
//! ```text
//!   /tmp/p25_forensics/run_<ts>_tg<tg>_<freq>/
//!     meta.json          run-level metadata (call_id, freqs, build, etc.)
//!     hdl_dibits.bits    SDRTrunk-format MSB-first 4-per-byte dibits
//!     nid_ring.jsonl     control-side NID events captured during call
//!     log.jsonl          /api/log entries during call window (TBD)
//!     wideband.cs16      symlink-or-copy to wideband_iq_capture file
//! ```

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::broadcast;

use crate::app::grant_follower::{
    CallTrackerEvent, CallTrackerEventKind, CallTrackerEventTx,
};
use crate::app::wideband_iq_task::WidebandIqCaptureState;

/// Default RAM cap for dibit buffer. 8 MB / 0.25 byte per dibit
/// (4-per-byte packing) = 32 M dibits = ~110 minutes at 4800 sym/s.
/// Realistic call: tens of seconds, so 8 MB is comfortable.
const DEFAULT_DIBIT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Default wideband seconds. The `wideband_iq_capture` tasks caps at
/// 30 s; we set 30 s as default which covers any single P25 call.
const DEFAULT_WIDEBAND_SECONDS: f64 = 30.0;

/// Forensics run dirs land on the SD card (57 GB free typical) —
/// the wideband.cs16 inside each run is too big for tmpfs (a
/// 30-second capture at 4 MSPS is 480 MB, larger than the entire
/// /tmp partition at 492 MB). 2026-05-03 disk-redirect commit.
const FORENSICS_DIR: &str = "/mnt/sd/p25_forensics";

/// Public state for the forensics ring. Held as `Arc<ForensicsRing>`
/// in `AppState` and shared with:
///   * traffic dibit reader (calls `record_dma_words` on every wakeup)
///   * forensics task (subscribes to `CallTrackerEvent`)
///   * /api/forensics_* handlers (read+modify config + status)
///
/// Fast path: `record_dma_words` checks `active` (atomic) and returns
/// immediately when no call is being captured. Lock contention only
/// happens during an active capture (~3-30 s per call).
pub struct ForensicsRing {
    /// Lock-free fast-path gate. True when a CallOpen has fired and
    /// CallClose hasn't yet. The reader path checks this first.
    active: AtomicBool,
    /// Set by /api/forensics_arm. While false, CallOpen is ignored.
    armed: AtomicBool,
    /// Auto-rearm on CallClose? When true, the ring stays ready for
    /// the next call. When false, single-shot.
    auto_rearm: AtomicBool,
    /// Forensics-mode override: while armed, follow encrypted grants
    /// too (they have no audio but the LSM chain still produces
    /// dibits we can diff against the SW oracle). Grant follower
    /// reads this via `follow_encrypted_enabled()`.
    follow_encrypted: AtomicBool,
    /// Total dibits captured across all runs (lifetime stat).
    total_dibits_captured: AtomicU64,
    /// Total runs completed.
    total_runs: AtomicU64,
    /// Last run dir (for status endpoint + host-side scp pull).
    last_run_dir: Mutex<Option<PathBuf>>,

    /// Slow-path mutable state. Held only while a call is active.
    inner: Mutex<ForensicsInner>,
}

struct ForensicsInner {
    config: ForensicsConfig,
    state: ForensicsState,
}

#[derive(Clone)]
pub struct ForensicsConfig {
    pub dibit_max_bytes: usize,
    pub wideband_seconds: f64,
}

impl Default for ForensicsConfig {
    fn default() -> Self {
        Self {
            dibit_max_bytes: DEFAULT_DIBIT_MAX_BYTES,
            wideband_seconds: DEFAULT_WIDEBAND_SECONDS,
        }
    }
}

#[derive(Default)]
enum ForensicsState {
    #[default]
    Idle,
    Capturing(ActiveCapture),
}

struct ActiveCapture {
    run_dir: PathBuf,
    call_id: u64,
    started_unix_ms: u64,
    tg: u32,
    freq_hz: Option<u64>,
    encrypted: bool,
    /// Packed dibits, 4 per byte, MSB-first (SDRTrunk format).
    /// Compatible with `tools/p25_dibit_diff.py`.
    dibit_buf: Vec<u8>,
    /// Dibits accumulated in the partially-filled byte (0..=3).
    dibit_partial_acc: u8,
    dibit_partial_count: u8,
    /// Dibit count this run. Lets us detect when we hit the max.
    dibit_count: u64,
    /// Wideband IQ capture filename (set by trigger_wideband()).
    wideband_remote_path: Option<PathBuf>,
    /// Did we hit the dibit cap and start dropping?
    dibits_truncated: bool,
}

/// Module-level switch read by `grant_follower` to decide whether
/// to reject encrypted grants. Lifted to a static so the follower
/// doesn't need an Arc<ForensicsRing> in its spawn args. Toggled
/// only by `ForensicsRing::arm/disarm`.
static FOLLOW_ENCRYPTED: AtomicBool = AtomicBool::new(false);

/// True when forensics is armed AND the operator opted in to
/// following encrypted grants for diff testing. Grant follower
/// checks this in its encrypted-rejection branch.
pub fn follow_encrypted_enabled() -> bool {
    FOLLOW_ENCRYPTED.load(Ordering::Acquire)
}

impl ForensicsRing {
    pub fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            armed: AtomicBool::new(false),
            auto_rearm: AtomicBool::new(false),
            follow_encrypted: AtomicBool::new(false),
            total_dibits_captured: AtomicU64::new(0),
            total_runs: AtomicU64::new(0),
            last_run_dir: Mutex::new(None),
            inner: Mutex::new(ForensicsInner {
                config: ForensicsConfig::default(),
                state: ForensicsState::Idle,
            }),
        }
    }

    pub async fn arm(
        &self,
        config: ForensicsConfig,
        auto_rearm: bool,
        follow_encrypted: bool,
    ) -> std::io::Result<()> {
        std::fs::create_dir_all(FORENSICS_DIR)?;
        let mut inner = self.inner.lock().await;
        inner.config = config;
        drop(inner);
        self.armed.store(true, Ordering::Release);
        self.auto_rearm.store(auto_rearm, Ordering::Release);
        self.follow_encrypted.store(follow_encrypted, Ordering::Release);
        FOLLOW_ENCRYPTED.store(follow_encrypted, Ordering::Release);
        tracing::info!(
            "forensics: armed (auto_rearm={auto_rearm}, \
             follow_encrypted={follow_encrypted}, dibit_max_bytes={}, \
             wideband_seconds={})",
            self.config_dibit_max().await,
            self.config_wideband_secs().await,
        );
        Ok(())
    }

    pub async fn disarm(&self) {
        self.armed.store(false, Ordering::Release);
        self.auto_rearm.store(false, Ordering::Release);
        self.follow_encrypted.store(false, Ordering::Release);
        FOLLOW_ENCRYPTED.store(false, Ordering::Release);
        tracing::info!("forensics: disarmed");
    }

    async fn config_dibit_max(&self) -> usize {
        self.inner.lock().await.config.dibit_max_bytes
    }
    async fn config_wideband_secs(&self) -> f64 {
        self.inner.lock().await.config.wideband_seconds
    }

    /// Snapshot for /api/forensics_status.
    pub async fn status(&self) -> ForensicsStatus {
        let armed = self.armed.load(Ordering::Acquire);
        let auto_rearm = self.auto_rearm.load(Ordering::Acquire);
        let follow_encrypted = self.follow_encrypted.load(Ordering::Acquire);
        let active = self.active.load(Ordering::Acquire);
        let last_run_dir = self.last_run_dir.lock().await.clone();
        let inner = self.inner.lock().await;
        let active_call_meta = match &inner.state {
            ForensicsState::Capturing(c) => Some(ActiveCallMeta {
                call_id: c.call_id,
                tg: c.tg,
                freq_hz: c.freq_hz,
                encrypted: c.encrypted,
                started_unix_ms: c.started_unix_ms,
                dibit_count: c.dibit_count,
                dibit_max_bytes: inner.config.dibit_max_bytes,
                dibits_truncated: c.dibits_truncated,
                wideband_remote_path: c.wideband_remote_path
                    .as_ref().map(|p| p.to_string_lossy().to_string()),
                run_dir: c.run_dir.to_string_lossy().to_string(),
            }),
            ForensicsState::Idle => None,
        };
        ForensicsStatus {
            armed,
            auto_rearm,
            follow_encrypted,
            active,
            dibit_max_bytes: inner.config.dibit_max_bytes,
            wideband_seconds: inner.config.wideband_seconds,
            total_runs_completed: self.total_runs.load(Ordering::Acquire),
            total_dibits_captured:
                self.total_dibits_captured.load(Ordering::Acquire),
            last_run_dir: last_run_dir.map(|p| p.to_string_lossy().to_string()),
            active_call: active_call_meta,
        }
    }

    /// Hot path: called from traffic_dibit_reader on every wakeup with
    /// the raw 64-bit DMA words. Returns immediately when not active.
    pub fn record_dma_words(&self, words: &[u64]) {
        if !self.active.load(Ordering::Relaxed) {
            return;
        }
        // Slow path -- need the lock to mutate the buffer.
        // try_lock so a contended lock from /api/forensics_status can't
        // stall the reader (we'd just drop a wakeup's worth of dibits;
        // the next wakeup picks up).
        let mut inner = match self.inner.try_lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let max = inner.config.dibit_max_bytes;
        let cap = match &mut inner.state {
            ForensicsState::Capturing(c) => c,
            ForensicsState::Idle => return,
        };
        if cap.dibits_truncated {
            return;
        }
        for &word in words {
            for i in 0..32 {
                let d = ((word >> (i * 2)) & 0x03) as u8;
                cap.dibit_partial_acc =
                    (cap.dibit_partial_acc << 2) | (d & 0x03);
                cap.dibit_partial_count += 1;
                if cap.dibit_partial_count == 4 {
                    if cap.dibit_buf.len() >= max {
                        cap.dibits_truncated = true;
                        return;
                    }
                    cap.dibit_buf.push(cap.dibit_partial_acc);
                    cap.dibit_partial_acc = 0;
                    cap.dibit_partial_count = 0;
                }
                cap.dibit_count += 1;
            }
        }
    }
}

#[derive(Serialize)]
pub struct ForensicsStatus {
    pub armed: bool,
    pub auto_rearm: bool,
    pub follow_encrypted: bool,
    pub active: bool,
    pub dibit_max_bytes: usize,
    pub wideband_seconds: f64,
    pub total_runs_completed: u64,
    pub total_dibits_captured: u64,
    pub last_run_dir: Option<String>,
    pub active_call: Option<ActiveCallMeta>,
}

#[derive(Serialize)]
pub struct ActiveCallMeta {
    pub call_id: u64,
    pub tg: u32,
    pub freq_hz: Option<u64>,
    pub encrypted: bool,
    pub started_unix_ms: u64,
    pub dibit_count: u64,
    pub dibit_max_bytes: usize,
    pub dibits_truncated: bool,
    pub wideband_remote_path: Option<String>,
    pub run_dir: String,
}

/// Spawn the forensics lifecycle task. Subscribes to
/// `CallTrackerEvent`s; on CallOpen (when armed) starts a capture; on
/// CallClose finalises and writes artefacts. Wideband IQ capture is
/// triggered programmatically in parallel.
pub fn spawn_forensics_task(
    forensics: Arc<ForensicsRing>,
    tracker_tx: CallTrackerEventTx,
    wideband: Arc<WidebandIqCaptureState>,
    ip_core: Arc<Mutex<crate::hardware::fpga::IpCore>>,
    build_tag: &'static str,
) {
    tokio::spawn(async move {
        let mut rx = tracker_tx.subscribe();
        tracing::info!("forensics task started (build {build_tag})");
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    if let Err(e) = handle_event(
                        &forensics, &wideband, &ip_core, build_tag, ev,
                    ).await {
                        tracing::warn!("forensics handle_event: {e}");
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("forensics: tracker rx lagged by {n}");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    tracing::info!("forensics: tracker tx closed; exiting");
                    return;
                }
            }
        }
    });
}

async fn handle_event(
    forensics: &Arc<ForensicsRing>,
    wideband: &Arc<WidebandIqCaptureState>,
    ip_core: &Arc<Mutex<crate::hardware::fpga::IpCore>>,
    build_tag: &'static str,
    ev: CallTrackerEvent,
) -> anyhow::Result<()> {
    match &ev.kind {
        CallTrackerEventKind::CallOpen {
            tg, freq_hz, encrypted, ..
        } => {
            if !forensics.armed.load(Ordering::Acquire) {
                return Ok(());
            }
            let run_id = format!(
                "run_{}_tg{}_{}",
                ev.timestamp_unix_ms / 1000,
                tg,
                freq_hz.unwrap_or(0),
            );
            let run_dir = std::path::Path::new(FORENSICS_DIR).join(&run_id);
            std::fs::create_dir_all(&run_dir)?;

            let mut inner = forensics.inner.lock().await;
            // Pre-allocate to dibit cap to avoid mid-call reallocs.
            let max_bytes = inner.config.dibit_max_bytes;
            let wideband_secs = inner.config.wideband_seconds;
            inner.state = ForensicsState::Capturing(ActiveCapture {
                run_dir: run_dir.clone(),
                call_id: ev.call_id,
                started_unix_ms: ev.timestamp_unix_ms,
                tg: *tg,
                freq_hz: *freq_hz,
                encrypted: *encrypted,
                dibit_buf: Vec::with_capacity(max_bytes.min(2 * 1024 * 1024)),
                dibit_partial_acc: 0,
                dibit_partial_count: 0,
                dibit_count: 0,
                wideband_remote_path: None,
                dibits_truncated: false,
            });
            drop(inner);
            // Mark active LAST so the reader path can't tee dibits
            // into a stale state.
            forensics.active.store(true, Ordering::Release);

            // Fire wideband IQ capture in parallel. Errors here don't
            // abort the dibit capture -- we still get useful HDL state
            // even without the matching wideband.
            match wideband.start(wideband_secs).await {
                Ok(path) => {
                    let core = ip_core.lock().await;
                    core.set_wideband_iq_dma_enable(true);
                    drop(core);
                    let mut inner = forensics.inner.lock().await;
                    if let ForensicsState::Capturing(c) = &mut inner.state {
                        c.wideband_remote_path = Some(path.clone());
                    }
                    tracing::info!(
                        "forensics: CallOpen tg={tg} freq={:?} -- wideband \
                         capture armed -> {:?}", freq_hz, path);
                }
                Err(e) => tracing::warn!(
                    "forensics: wideband start failed: {e}"),
            }
        }
        CallTrackerEventKind::CallClose { .. } => {
            if !forensics.active.load(Ordering::Acquire) {
                return Ok(());
            }
            // Stop teeing dibits FIRST; reader's fast-path check sees
            // the new value before we lock to finalise.
            forensics.active.store(false, Ordering::Release);

            // Take ownership of the active capture out of the mutex.
            let cap = {
                let mut inner = forensics.inner.lock().await;
                match std::mem::take(&mut inner.state) {
                    ForensicsState::Capturing(c) => c,
                    ForensicsState::Idle => return Ok(()),
                }
            };

            // Wait briefly for wideband to finish so the cs16 file is
            // closed by the time the host scp's it. wideband_iq_task
            // caps captures at the configured seconds; drain ~+1 s.
            let drain_deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < drain_deadline {
                let snap = wideband.snapshot().await;
                if snap.active.is_none() {
                    break;
                }
                tokio::time::sleep(
                    std::time::Duration::from_millis(200)).await;
            }
            // Disable wideband DMA so the next idle period doesn't
            // burn power. The next CallOpen will re-enable.
            {
                let core = ip_core.lock().await;
                core.set_wideband_iq_dma_enable(false);
            }

            // Finalise and write artefacts.
            if let Err(e) = finalise_capture(&cap, &ev, build_tag).await {
                tracing::warn!("forensics: finalise failed: {e}");
            }

            forensics.total_dibits_captured
                .fetch_add(cap.dibit_count, Ordering::AcqRel);
            forensics.total_runs.fetch_add(1, Ordering::AcqRel);
            *forensics.last_run_dir.lock().await = Some(cap.run_dir.clone());

            // Auto re-arm: ring stays armed, ready for next CallOpen.
            // If auto_rearm is off, disarm now.
            if !forensics.auto_rearm.load(Ordering::Acquire) {
                forensics.armed.store(false, Ordering::Release);
                tracing::info!("forensics: single-shot disarm after run");
            }
        }
        _ => {}
    }
    Ok(())
}

async fn finalise_capture(
    cap: &ActiveCapture,
    close_ev: &CallTrackerEvent,
    build_tag: &'static str,
) -> std::io::Result<()> {
    use std::io::Write;

    // Flush partial byte (if any) so no dibits are lost. Left-justify
    // residual into the high bits, matching the host-side packer.
    let mut packed = cap.dibit_buf.clone();
    if cap.dibit_partial_count != 0 {
        let acc = cap.dibit_partial_acc << ((4 - cap.dibit_partial_count) * 2);
        packed.push(acc);
    }

    let dibits_path = cap.run_dir.join("hdl_dibits.bits");
    std::fs::write(&dibits_path, &packed)?;

    // meta.json
    let meta_path = cap.run_dir.join("meta.json");
    let close_reason = match &close_ev.kind {
        CallTrackerEventKind::CallClose { reason, .. } =>
            format!("{:?}", reason),
        _ => "unknown".into(),
    };
    let last_upd = match &close_ev.kind {
        CallTrackerEventKind::CallClose { last_upd_at_unix_ms, .. } =>
            *last_upd_at_unix_ms,
        _ => 0,
    };
    let meta = serde_json::json!({
        "schema":              "p25-forensics-v1",
        "build_tag":           build_tag,
        "call_id":             cap.call_id,
        "tg":                  cap.tg,
        "freq_hz":             cap.freq_hz,
        "encrypted":           cap.encrypted,
        "started_unix_ms":     cap.started_unix_ms,
        "ended_unix_ms":       close_ev.timestamp_unix_ms,
        "last_upd_unix_ms":    last_upd,
        "close_reason":        close_reason,
        "dibit_count":         cap.dibit_count,
        "dibits_truncated":    cap.dibits_truncated,
        "dibit_buf_bytes":     packed.len(),
        "wideband_remote":     cap.wideband_remote_path
            .as_ref().map(|p| p.to_string_lossy().to_string()),
    });
    let mut f = std::fs::File::create(&meta_path)?;
    writeln!(f, "{}", serde_json::to_string_pretty(&meta).unwrap())?;

    // FINDINGS.md template so the host side has a record skeleton.
    let findings_path = cap.run_dir.join("FINDINGS.md");
    std::fs::write(&findings_path, format!(
        "# Forensics run {}\n\n\
         Captured on-device {} (build `{}`).\n\n\
         - TG: {}\n- Freq: {:?} Hz\n- Encrypted: {}\n\
         - Dibit count: {} ({} bytes packed){}\n\
         - Wideband IQ: `{}`\n\n\
         ## Next step\n\n\
         Run [`tools/p25_chain_compare.py`](../../tools/p25_chain_compare.py) \
         on this dir to diff HDL dibits vs SW oracle.\n",
        cap.run_dir.file_name().unwrap_or_default().to_string_lossy(),
        cap.started_unix_ms, build_tag, cap.tg, cap.freq_hz, cap.encrypted,
        cap.dibit_count, packed.len(),
        if cap.dibits_truncated { " [TRUNCATED at cap]" } else { "" },
        cap.wideband_remote_path.as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "(none)".into()),
    ))?;

    tracing::info!(
        "forensics: finalised {} ({} dibits, {} bytes, wideband={:?})",
        cap.run_dir.display(), cap.dibit_count, packed.len(),
        cap.wideband_remote_path);
    Ok(())
}
