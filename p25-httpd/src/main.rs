//! Fishball P25 Trunking Radio - PS Application
//!
//! Runs on the Zynq-7020 ARM cores. Responsibilities:
//! - Configure AD9361 via IIO
//! - Configure FPGA DDC + demod via UIO registers
//! - Read dibit DMA stream from FPGA
//! - Decode P25 control channel (sync, TSBK parsing, state machine)
//! - Serve web UI for monitoring talkgroups and grants

use std::sync::Arc;

use clap::Parser;
use tokio::sync::{broadcast, RwLock};

#[cfg(target_os = "linux")]
mod fpga;
mod httpd;
#[cfg(target_os = "linux")]
mod iio;
mod audio;
mod event_log;
mod lsm;
mod monitor;
mod p25;
mod vocoder;
mod jmbe;
#[cfg(target_os = "linux")]
mod rxbuffer;
#[cfg(target_os = "linux")]
mod uio;

use p25::control_channel::ControlChannelDecoder;

/// Build tag, logged at startup and exposed via `/api/system`.
///
/// **Bump this string whenever a feature flag changes** so on-target
/// "is the binary I just flashed actually the one I just built?" is a
/// trivial check (`grep "p25-httpd build" /var/log/p25-httpd.log` or
/// `wget -qO- http://target:8080/api/system | grep build`). Don't try
/// to be clever with mtimes (Buildroot zeros them) or doc-comment
/// strings (they don't survive into the binary).
pub const BUILD_TAG: &str = "2026-04-15-tdu-lc-burst-fix";

/// Cumulative + snapshot stats for the HDL LSM chain (Phase 6E PL
/// gateware). Populated by the HDL LSM heartbeat task and read by
/// `/api/hdl_lsm`. Single source of truth for everything the
/// heartbeat task used to keep in task-local variables.
#[derive(Debug, Clone, Default)]
pub struct HdlLsmRuntime {
    pub started_at: Option<std::time::Instant>,
    pub last_tick_at: Option<std::time::Instant>,

    // Last raw register snapshot (read every 16 ms).
    pub pll_dbg: i16,
    pub sp_dbg: i16,
    pub sync_distance: u8,
    pub bch_busy: bool,
    pub in_nid_window: bool,
    pub dibit_overflow_latched: bool,
    pub iq_overflow_latched: bool,
    pub last_nac: u16,
    pub last_duid: u8,
    pub last_drop_count: u16,

    // Last successful NID event.
    pub last_nid_at: Option<std::time::Instant>,
    pub last_nid_valid: bool,
    pub last_nid_n_errors: u8,

    // Cumulative counters since boot.
    pub total_nid_events: u64,
    pub valid_nid_events: u64,
    pub dibit_overflow_ticks: u64,
    pub iq_overflow_ticks: u64,

    // NAC histogram across all NID events. Winner = locked site ID.
    pub nac_hist: std::collections::HashMap<u16, u64>,

    // Last completed heartbeat window snapshot (1 s cadence).
    pub hb_pll_min: i16,
    pub hb_pll_max: i16,
    pub hb_sp_min: i16,
    pub hb_sp_max: i16,
    pub hb_sync_dist_best: u8,
    pub hb_bch_busy_ticks: u32,
    pub hb_in_window_ticks: u32,
    pub hb_nid_event_ticks: u32,
    pub hb_dibit_overflow_ticks: u32,
    pub hb_iq_overflow_ticks: u32,
    pub hb_iq_kbps: u64,
    pub hb_iq_buf_rolls: u32,
    pub hb_window_valid_count: u32,
    pub hb_window_event_count: u32,

    // Last 32 NID events (chronological, oldest first after wrap).
    pub nid_ring: Vec<HdlNidEntry>,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct HdlNidEntry {
    pub seq: u64,
    pub t_ms_since_boot: u64,
    pub nac: u16,
    pub duid: u8,
    pub valid: bool,
    pub n_errors: u8,
    pub sync_distance: u8,
    pub drop_count: u16,
    pub pll_dbg: i16,
    pub sp_dbg: i16,
}

impl HdlLsmRuntime {
    pub fn top_nacs(&self, n: usize) -> Vec<(u16, u64)> {
        let mut v: Vec<(u16, u64)> = self
            .nac_hist
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

/// Per-source IRQ counters maintained by the InterruptHandler task.
#[derive(Debug, Clone, Copy, Default)]
pub struct IrqStats {
    pub total: u64,
    pub dibit: u64,
    pub traffic: u64,
    pub iq: u64,
    pub lsm_dibit: u64,
    /// Phase 7A.2: traffic-side LSM dibit DMA wakeups.
    pub traffic_lsm_dibit: u64,
    pub last_at_secs_ago: f64,
    /// Set to None until the first IRQ; updated only by the IRQ task.
    pub started_at: Option<std::time::Instant>,
    pub last_at: Option<std::time::Instant>,
}

/// Phase 7A.1: data-side counters for the traffic DMA path. Distinct
/// from `IrqStats.traffic` (which counts wakeups) -- this struct
/// tracks the bytes / dibits actually consumed by the traffic dibit
/// reader task. Both are exposed via `/api/traffic`.
///
/// At Phase 7A.1 the traffic chain is C4FM-only and Clay County is
/// LSM, so the dibit *content* is expected garbage; we are only
/// validating that the chain comes alive when the DDC is retuned.
/// The histogram is included for sanity (a dead chain produces all
/// zeros; a live chain produces a roughly even spread across all 4
/// dibits even on garbage). Phase 7A.2 will add an LSM traffic chain
/// that produces decodable content; once that's in, the histogram
/// will skew toward the C4FM all-zero pattern on dead air and the
/// LSM-decoded dibit pattern on active calls.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrafficStats {
    pub started_at: Option<std::time::Instant>,
    pub last_at: Option<std::time::Instant>,
    pub wakeups: u64,
    pub total_buffers: u64,
    pub total_bytes: u64,
    pub total_dibits: u64,
    pub dibit_hist: [u64; 4],
    // ── Phase 7C: IMBE frame extraction counters ──────────────────
    //
    // Updated by the `ImbeForwarder` voice handler that's wired into
    // the `traffic_lsm_decoder` instance. Each successful LDU1/LDU2
    // body extraction yields 9 IMBE frames (~180 ms of audio at 50
    // frames/sec when locked). Phase 7D will consume these frames
    // from a separate mpsc channel and produce PCM audio; for 7C
    // these counters are the only observable proof that the IMBE
    // extraction pipeline is alive.
    pub hdu_count: u64,
    pub ldu1_count: u64,
    pub ldu2_count: u64,
    pub tdu_count: u64,
    pub tdu_lc_count: u64,
    /// Total IMBE frames pushed to the (future) Phase 7D vocoder
    /// channel. Should equal `(ldu1_count + ldu2_count) * 9` in
    /// steady state -- any divergence indicates a frame extraction
    /// failure (e.g. wrong dibit count, status-strip math off).
    pub imbe_frames_extracted: u64,
    /// Wall-clock instant of the most recent IMBE frame batch.
    /// Used to compute "frames per second" for the dashboard.
    pub last_imbe_at: Option<std::time::Instant>,
}

/// Phase 7D: voice frame handler that counts IMBE events AND forwards
/// raw frames to the vocoder task via an mpsc channel.
///
/// Implements `p25::control_channel::VoiceHandler`. Installed on
/// the `traffic_lsm_decoder` via `set_voice_handler`. Held as
/// `Arc<dyn VoiceHandler + Send + Sync>`.
///
/// Uses `try_send` (non-async) on the mpsc channel because the
/// `VoiceHandler` trait methods take `&self` and are called from
/// synchronous `process_dibit` code inside a tokio task. If the
/// channel is full the frame batch is dropped and `imbe_frames_dropped`
/// is incremented -- the vocoder task is expected to keep up at
/// ~50 frames/sec (one LDU every ~180 ms).
pub struct ImbeForwarder {
    pub hdu_count: std::sync::atomic::AtomicU64,
    pub ldu1_count: std::sync::atomic::AtomicU64,
    pub ldu2_count: std::sync::atomic::AtomicU64,
    pub tdu_count: std::sync::atomic::AtomicU64,
    pub tdu_lc_count: std::sync::atomic::AtomicU64,
    pub imbe_frames_extracted: std::sync::atomic::AtomicU64,
    pub imbe_frames_dropped: std::sync::atomic::AtomicU64,
    /// Phase 7F.3 (2026-04-14): count of LDU frame batches the
    /// forwarder refused to hand to the vocoder because
    /// `current_talkgroup == 0` (follower is Idle). These are
    /// framer false-positives extracted from residual dibits on the
    /// traffic DDC between calls -- the source of the "TG=0 phantom
    /// call" events in the log.
    pub imbe_frames_dropped_idle: std::sync::atomic::AtomicU64,
    pub last_imbe_at_millis: std::sync::atomic::AtomicU64,
    /// Vocoder stats -- updated by the vocoder task, read by /api/traffic.
    pub vocoder_pcm_produced: std::sync::atomic::AtomicU64,
    pub vocoder_errors: std::sync::atomic::AtomicU64,
    pub vocoder_frames_encrypted: std::sync::atomic::AtomicU64,
    /// Set by the grant follower task when it locks onto a TG. The
    /// vocoder task reads this to skip encrypted calls.
    pub call_encrypted: std::sync::atomic::AtomicBool,
    /// Current talkgroup (set by grant follower, read by vocoder
    /// to tag AudioChunks). 0 = idle / unknown.
    pub current_talkgroup: std::sync::atomic::AtomicU16,
    /// TGs that have ever been observed encrypted. Once a TG is in
    /// this set, the follower defaults to encrypted even if the
    /// current grant doesn't carry service options.
    pub encrypted_tg_history: std::sync::Mutex<std::collections::HashSet<u16>>,
    /// Set by the grant follower on call boundary (new TG lock or
    /// Idle→Active). The vocoder task checks this and resets mbelib
    /// state to avoid cross-call artifacts.
    pub vocoder_reset_pending: std::sync::atomic::AtomicBool,
    /// Ring buffer of the last N raw IMBE frames for diagnostic capture
    /// via `/api/imbe_dump`. Stores (talkgroup, encrypted, frame_bytes).
    pub imbe_ring: std::sync::Mutex<Vec<(u16, bool, [u8; 18])>>,
    /// Channel to the vocoder task. Each send is a batch of 9 frames
    /// (one LDU's worth = 180 ms of audio).
    imbe_tx: tokio::sync::mpsc::Sender<[p25::voice_frame::ImbeFrameRaw; 9]>,
}

impl ImbeForwarder {
    pub fn new(
        imbe_tx: tokio::sync::mpsc::Sender<[p25::voice_frame::ImbeFrameRaw; 9]>,
    ) -> Self {
        Self {
            hdu_count: 0.into(),
            ldu1_count: 0.into(),
            ldu2_count: 0.into(),
            tdu_count: 0.into(),
            tdu_lc_count: 0.into(),
            imbe_frames_extracted: 0.into(),
            imbe_frames_dropped: 0.into(),
            imbe_frames_dropped_idle: 0.into(),
            last_imbe_at_millis: 0.into(),
            vocoder_pcm_produced: 0.into(),
            vocoder_errors: 0.into(),
            vocoder_frames_encrypted: 0.into(),
            call_encrypted: false.into(),
            current_talkgroup: 0.into(),
            encrypted_tg_history: std::sync::Mutex::new(std::collections::HashSet::new()),
            vocoder_reset_pending: false.into(),
            imbe_ring: std::sync::Mutex::new(Vec::with_capacity(128)),
            imbe_tx,
        }
    }

    fn touch_imbe(&self, n_frames: u64) {
        use std::sync::atomic::Ordering;
        self.imbe_frames_extracted.fetch_add(n_frames, Ordering::Relaxed);
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_imbe_at_millis.store(now_millis, Ordering::Relaxed);
    }

    fn forward_frames(&self, frames: &[p25::voice_frame::ImbeFrameRaw; 9]) {
        use std::sync::atomic::Ordering;
        let tg = self.current_talkgroup.load(Ordering::Relaxed);
        let enc = self.call_encrypted.load(Ordering::Relaxed);

        // Phase 7F.3 (2026-04-14): drop frames that arrive while the
        // follower is Idle (current_talkgroup == 0). These are
        // framer false-positives: the traffic LSM HDL chain keeps
        // producing dibits from whatever the NCO is still pointed
        // at between calls, and the software framer happily
        // extracts "LDUs" out of that noise and dispatches them
        // here. Pushing them to the vocoder produces the "TG=0
        // phantom call" pattern we saw in the event log (e.g.
        // `call_end TG=0 frames=18 pcm=2880 (10278 ms)` --
        // 18 frames decoded as clear over a 10 s window with no
        // actual call in progress).
        //
        // Drop them on the floor and count them so the dashboard
        // can show the rate. The diagnostic ring buffer still
        // records them (tagged tg=0) so `/api/imbe_dump` can be
        // used to inspect what the framer was pulling out.
        if let Ok(mut ring) = self.imbe_ring.lock() {
            for f in frames {
                if ring.len() >= 128 {
                    ring.remove(0);
                }
                ring.push((tg, enc, f.bits));
            }
        }
        if tg == 0 {
            self.imbe_frames_dropped_idle
                .fetch_add(9, Ordering::Relaxed);
            return;
        }

        match self.imbe_tx.try_send(*frames) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.imbe_frames_dropped.fetch_add(9, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.imbe_frames_dropped.fetch_add(9, Ordering::Relaxed);
            }
        }
    }
}

impl p25::control_channel::VoiceHandler for ImbeForwarder {
    fn on_ldu1(&self, frames: &[p25::voice_frame::ImbeFrameRaw; 9]) {
        use std::sync::atomic::Ordering;
        self.ldu1_count.fetch_add(1, Ordering::Relaxed);
        self.touch_imbe(9);
        self.forward_frames(frames);
    }

    fn on_ldu2(&self, frames: &[p25::voice_frame::ImbeFrameRaw; 9]) {
        use std::sync::atomic::Ordering;
        self.ldu2_count.fetch_add(1, Ordering::Relaxed);
        self.touch_imbe(9);
        self.forward_frames(frames);
    }

    fn on_hdu(&self) {
        self.hdu_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn on_tdu(&self) {
        self.tdu_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn on_tdu_lc(&self) {
        self.tdu_lc_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[derive(Parser)]
#[command(name = "p25-httpd", about = "Fishball P25 Trunking Radio")]
struct Args {
    /// HTTP listen address
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: String,

    /// AD9361 RX LO frequency in Hz
    #[arg(long, default_value_t = 858_100_000)]
    rx_lo: u64,

    /// AD9361 sample rate in Hz
    #[arg(long, default_value_t = 8_000_000)]
    sample_rate: u64,

    /// P25 control channel frequency in Hz
    #[arg(long, default_value_t = 860_962_500)]
    control_freq: u64,

    /// Pluto LO PPM offset for crystal calibration.
    ///
    /// Compensates the AD9361 crystal frequency error by shifting the
    /// DDC NCO (NOT the AD9361 LO request -- the LO synthesizer step
    /// at our operating range is much coarser than the typical PPM-
    /// scale shift, so a small LO shift gets rounded back to the
    /// nominal value while the NCO computation still moves, doubling
    /// the post-DDC offset and breaking lock. The DDC NCO is generated
    /// in fabric at 1 Hz precision and is the only place a sub-step
    /// shift can actually be applied).
    ///
    /// Negative ppm means the Pluto crystal is slow (real signals
    /// appear above their expected IF). For the Clay County test
    /// Pluto: -0.54 ppm. SDRTrunk's tuner panel exposes the same
    /// setting and is the reference for the value to use here.
    ///
    /// Math: nco_shift = -ppm * 1e-6 * rx_lo Hz. With rx_lo=858 MHz
    /// and ppm=-0.54, that is +463 Hz added to the nominal NCO.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    lo_ppm: f64,

    /// AD9361 RX hardware gain in dB. Sets gain_control_mode=manual and
    /// writes this value to `hardwaregain`.
    ///
    /// The Maia HDL LSM chain has no software AGC stage (SDRTrunk has one
    /// at `P25P1DemodulatorLSM.java:157-172`, a per-symbol IIR that
    /// normalises IQ magnitude to a fixed OBJECTIVE_MAGNITUDE, but we
    /// don't). Our slicer has fixed integer decision thresholds, so the
    /// analog front-end gain has to land in a narrow ±5-10 dB window
    /// around the slicer's expected amplitude or the outer 4FSK symbols
    /// get clipped (gain too high) or crowded into the inner bins (gain
    /// too low). The AD9361 AGC in both `slow_attack` and `fast_attack`
    /// modes does NOT converge to this window on a strong antenna --
    /// slow_attack lands around 71-73 dB (too high), fast_attack lands
    /// around 0 dB (too low). Manual gain at 55-60 dB on the Clay County
    /// test target hits 96-97 % NID success, 72-75 % TSBK CRC pass,
    /// 20+ msgs/sec -- above the doc 029 historical target.
    ///
    /// Default 60 dB was measured on 2026-04-15 with the user's current
    /// antenna. Re-tune via this CLI arg or via `/api/reinit?gain_db=N`
    /// if the antenna / site changes. A proper software AGC in the HDL
    /// chain would eliminate the per-antenna tuning -- see
    /// doc/changes/040_api_reinit_and_manual_gain.md.
    #[arg(long, default_value_t = 60.0)]
    hardwaregain: f64,

    /// AD9361 RX analog front-end filter bandwidth in Hz.
    ///
    /// Default 4 MHz. The Maia DDC stage 1 FIR (48 taps, 200 kHz
    /// cutoff, Kaiser β=6) does not have enough stopband rejection
    /// at 500 kHz-2 MHz offset to handle wider rf_bandwidth in the
    /// presence of adjacent P25 emitters (e.g. the Clay County site
    /// has P25 carriers at 860.0 MHz and 859.35 MHz that leak through
    /// stage 1 at rf_bandwidth >= 5 MHz and crush the control-channel
    /// CRC pass rate from ~70 % to ~40 % at 5 MHz and ~15 % at 6-8 MHz.
    /// See `project_p25_ddc_stage1_filter_weak.md` memory for the
    /// live sweep measurement, and
    /// `doc/changes/040_api_reinit_and_manual_gain.md` for the full
    /// investigation.
    ///
    /// **This default will change to 8 MHz once the DDC stage 1 FIR
    /// is reworked** with deeper adjacent-channel rejection (more
    /// taps, higher β, or a pre-decimator before stage 1). Until then
    /// 4 MHz is the only usable production value on sites with
    /// adjacent emitters inside ±2 MHz of the control channel.
    #[arg(long, default_value_t = 4_000_000)]
    rf_bandwidth: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Default log level: info for our crate, warn for everything else.
    // Honour RUST_LOG when set, otherwise emit a sensible default so the
    // user actually sees the dibit reader / IRQ / decoder logs without
    // having to manually configure tracing.
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,p25_httpd=info"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .init();

    let args = Args::parse();

    // Build marker. Bump the BUILD_TAG string whenever a feature flag changes
    // so on-target verification of "is this binary the one I just built" is a
    // single grep instead of guessing from mtimes (Buildroot zeros mtimes to
    // 1970) or doc-comment strings (which don't survive into the binary).
    //
    // BUILD_TAG also lands in /api/system as the `build` field so the browser
    // can show the deployed build at a glance.
    tracing::info!(
        "p25-httpd build: {} (dashboard_source=lsm_decoder, Phase 6F.1)",
        BUILD_TAG
    );

    // Pluto crystal calibration: shift the DDC NCO by -ppm * 1e-6 * rx_lo Hz.
    // See the doc comment on Args::lo_ppm for why this only moves the NCO and
    // not the LO request. nco_lo_shift_hz is folded into nco_offset further
    // down inside the cfg(linux) block where the IP core gets configured.
    let nco_lo_shift_hz = -args.lo_ppm * 1e-6 * args.rx_lo as f64;

    tracing::info!(
        "Fishball P25 starting: RX LO={} Hz, control_freq={} Hz, lo_ppm={:+} ({:+.1} Hz NCO shift)",
        args.rx_lo,
        args.control_freq,
        args.lo_ppm,
        nco_lo_shift_hz
    );

    let (event_tx, _) = broadcast::channel::<String>(256);
    let mut decoder = ControlChannelDecoder::new();
    decoder.set_event_tx(event_tx.clone());
    let decoder = Arc::new(RwLock::new(decoder));

    // Phase 6E.10 bring-up: a second independent `ControlChannelDecoder`
    // instance fed by the HDL LSM dibit stream from `lsm_dibit_dma`.
    // Runs the identical Hunting -> ReadingNid -> ReadingDu -> trellis
    // -> CRC -> TsbkMessage pipeline as the C4FM decoder above, just
    // against a different dibit source. Shares the same event_tx
    // broadcast channel so both decoders' TSBKs land on the same
    // dashboard WebSocket (with distinct trace targets in the server
    // log so they can be separated post-hoc). Both instances operate
    // in parallel against the same RF capture on bring-up days -- this
    // is how we validate the HDL LSM port (Phase 6E.0-6E.9) against the
    // working Phase 2A C4FM path.
    // Phase 7B: typed grant event channel + monitor list.
    let (grant_event_tx, mut grant_event_rx) =
        tokio::sync::mpsc::channel::<p25::events::P25Event>(128);
    let monitor_list = Arc::new(RwLock::new(monitor::MonitorList::default()));

    let mut lsm_decoder = ControlChannelDecoder::new();
    lsm_decoder.set_event_tx(event_tx.clone());
    lsm_decoder.set_grant_event_tx(grant_event_tx);
    let lsm_decoder = Arc::new(RwLock::new(lsm_decoder));

    // Phase 9 retirement (2026-04-15): the Phase 6D `iq_lsm_decoder`
    // has been removed. It was a pure-software LSM demod + TSBK
    // framer pipeline built in Phase 6D as the "algorithm
    // development + validation reference", BEFORE Phase 6E ported
    // the full LSM demod into Amaranth gateware. Since Phase 6E.9
    // (HDL LSM chain on the control DDC) went green, the HDL path
    // has been the production decoder and the software pipeline
    // has been pure dead weight -- an ARM-CPU-expensive cross-check
    // that never reveals anything the HDL path doesn't already
    // surface. Phase 9 formally retires it.
    //
    // What went with it:
    //   - 200-line Phase 6D LSM IQ reader tokio task (read
    //     iq_dma -> LsmPipeline -> process_directed_tsdu). Gone.
    //   - `LsmStats` + `/api/lsm` endpoint. Gone.
    //   - Dashboard "LSM Pipeline (Phase 6D)" card + four
    //     `ps_iq_lsm` / `ps_phase6d` columns in the decoder-compare
    //     matrix. Gone.
    //   - `iq_dma` HDL ring stays in the bitstream for now
    //     (dormant dead weight, ~few hundred LUT + one M_AXI_HP
    //     channel) but is not enabled by the PS at boot any more.
    //
    // See doc/changes/039 for the retirement rationale and the
    // inventory of what was removed.

    // Phase 7C: fourth `ControlChannelDecoder` instance fed by the
    // new `traffic_lsm_dibit_dma` ring (Phase 7A.2 HDL chain). Unlike
    // the three control-channel decoders above, this one runs on the
    // FOLLOWED VOICE CHANNEL and produces HDU/LDU1/LDU2/TDU/TDU_LC
    // events instead of TSDUs. The voice handler installed below
    // forwards extracted IMBE frames to a counter on `TrafficStats`
    // (Phase 7C) and will forward to the vocoder mpsc channel in
    // Phase 7D.
    //
    // The decoder runs the same Hunting -> ReadingNid -> ReadingDataUnit
    // state machine as the control side, just with the new LDU/HDU/TDU
    // dispatch arms in `process_dibit` (added in Phase 7C) doing the
    // work instead of the TSDU dispatch arm.
    // Phase 7D: mpsc channel for IMBE frame batches from the voice
    // handler to the vocoder task. Buffer 16 LDU batches (~2.9 s of
    // audio) to absorb jitter without dropping.
    let (imbe_tx, imbe_rx) =
        tokio::sync::mpsc::channel::<[p25::voice_frame::ImbeFrameRaw; 9]>(16);
    let imbe_forwarder = Arc::new(ImbeForwarder::new(imbe_tx));

    let mut traffic_lsm_decoder = ControlChannelDecoder::new();
    traffic_lsm_decoder.set_event_tx(event_tx.clone());
    // Phase 7D: install the IMBE forwarder as the decoder's voice
    // handler. Counts events AND pushes frame batches to the vocoder
    // task via try_send.
    traffic_lsm_decoder.set_voice_handler(imbe_forwarder.clone());
    let traffic_lsm_decoder = Arc::new(RwLock::new(traffic_lsm_decoder));

    // Phase 7F.1 (2026-04-14): shared structured event log. Capacity
    // 1024 ≈ ~3-4 minutes of grant/traffic/imbe events on Clay County
    // at the observed ~5 grants/sec + per-LDU IMBE batches. Tuned so
    // the dashboard tab can show "recent history" without pagination
    // while staying well under typical PS memory budgets (1024 * ~400
    // bytes each = ~400 KB peak).
    let event_log = Arc::new(crate::event_log::EventLog::new(1024));
    event_log.push(
        crate::event_log::LogCategory::System,
        "p25-httpd startup",
        serde_json::json!({
            "build_tag": crate::BUILD_TAG,
        }),
    );

    // Phase 9 retirement: `lsm_stats` (the shared `LsmStats` mutex
    // for the Phase 6D software pipeline) is gone along with the
    // pipeline itself. The PL HDL LSM runtime stats below (`hdl_lsm`)
    // are the production source of truth for "is the LSM chain
    // alive / how many valid NIDs / what NACs" — they tap the HDL
    // register bank directly instead of recomputing from raw IQ.
    //
    // Phase 6F.2: shared PL HDL LSM runtime + IRQ stats, populated by
    // their respective tasks below and read by /api/hdl_lsm and
    // /api/irq_stats. Same out-of-cfg(linux) treatment.
    let hdl_lsm = Arc::new(tokio::sync::Mutex::new(HdlLsmRuntime::default()));
    let irq_stats = Arc::new(tokio::sync::Mutex::new(IrqStats::default()));

    // Phase 7A.1: traffic-channel grant follower + dibit-reader stats.
    // Created out of cfg(linux) so the AppState construction below sees
    // them on every target. The polling task that drives the
    // TrafficManager and the dibit reader task that updates TrafficStats
    // both live INSIDE the cfg(linux) block (they touch ip_core).
    //
    // Note: TrafficManager::new takes (rx_lo_hz, sample_rate_hz) so it
    // can compute NCO offsets at runtime; both come straight from the
    // CLI args and never change after startup.
    let traffic_manager = Arc::new(tokio::sync::Mutex::new(
        p25::traffic_manager::TrafficManager::new(args.rx_lo, args.sample_rate),
    ));
    let traffic_stats = Arc::new(tokio::sync::Mutex::new(TrafficStats::default()));
    // Phase 7A.1 manual control: when this is `false`, the grant
    // follower task skips its entire loop iteration (no grant snapshot,
    // no retune, no timeout sweep). The user can flip this off via
    // `GET /api/traffic?follower=off` to take manual control of the
    // traffic DDC NCO + demod_enable bits without the polling task
    // immediately yanking them back. Default is on; the state does NOT
    // persist across restarts (process-lifetime only).
    let traffic_follower_enabled =
        Arc::new(std::sync::atomic::AtomicBool::new(true));

    #[cfg(target_os = "linux")]
    let (ip_core, ad9361) = {
        use tokio::sync::Mutex;

        // 1. Initialize FPGA IP core via UIO
        let (ip_core, interrupt_handler) = fpga::IpCore::take().await?;
        tracing::info!("FPGA IP core initialized");

        // 2. Configure AD9361 via IIO.
        //
        // Manual gain is deliberate: the Maia HDL LSM chain has no
        // software AGC (unlike SDRTrunk's P25P1DemodulatorLSM, which
        // does a per-symbol IIR normalisation to OBJECTIVE_MAGNITUDE at
        // lines 157-172). Our slicer's decision thresholds are fixed
        // integer values in gateware, so the analog front-end gain has
        // to land in a narrow window (~55-60 dB on this antenna at the
        // Clay County test target) or the outer 4FSK symbols get
        // misclassified. AD9361 AGC in both slow_attack and fast_attack
        // modes converges OUTSIDE that window on a strong antenna --
        // slow_attack picks 71-73 dB, fast_attack picks ~0 dB. Both
        // produce ~3 % CRC pass; manual 60 dB produces ~75 % CRC pass.
        // See doc/changes/040 for the live measurement sweep.
        let ad9361 = iio::Ad9361::new().await?;
        ad9361.set_rx_lo_frequency(args.rx_lo).await?;
        ad9361
            .set_sampling_frequency(args.sample_rate as u32)
            .await?;
        ad9361.set_rx_rf_bandwidth(args.rf_bandwidth).await?;
        ad9361
            .set_rx_gain_mode(iio::GainMode::Manual)
            .await?;
        ad9361.set_rx_gain(args.hardwaregain).await?;
        tracing::info!(
            "AD9361 configured: LO={} Hz, Fs={} Hz, BW={} Hz, \
             gain_mode=manual, hardwaregain={} dB",
            args.rx_lo,
            args.sample_rate,
            args.rf_bandwidth,
            args.hardwaregain,
        );

        // 3. Configure control channel DDC (FIR filters + decimation + NCO).
        // The lo_ppm crystal calibration is folded into the NCO here -- see
        // the doc comment on Args::lo_ppm for the rationale and the math.
        let nco_offset =
            args.control_freq as f64 - args.rx_lo as f64 + nco_lo_shift_hz;
        ip_core.configure_ddc(nco_offset, args.sample_rate as f64)?;
        ip_core.set_ddc_enable(true);
        // Ring DMA: enable bit is level-triggered, starts continuous writes
        ip_core.set_demod_enable(true);
        // Phase 9 retirement: the post-DDC IQ ring DMA
        // (`iq_dma_enable`) was fed the Phase 6D software LSM
        // pipeline. That pipeline is gone, so we leave the ring
        // master disabled at boot. The HDL block is still present
        // in the bitstream (dormant dead weight) so a future phase
        // can re-enable it if we need a raw-IQ tap again -- e.g.,
        // for on-target baseband capture to disk, or for a new
        // in-PL DSP block that taps post-DDC IQ.
        ip_core.set_iq_dma_enable(false);
        // Phase 6E.9/6E.10: enable the HDL LSM demod chain (runs alongside
        // the C4FM demod on the same control DDC output) and its dedicated
        // dibit ring DMA. NID events themselves are PS-polled via
        // lsm_status below.
        // Phase 6G.1: also turn on the front-end DC blocker -- this is
        // the production-correct state and removes the slow IQ DC bias
        // that otherwise gives the slicer a 60/40 inner/outer dibit
        // ratio for 2-3 minutes after PLL start. See doc/changes/031.
        ip_core.set_lsm_enable(true);
        ip_core.set_lsm_dibit_dma_enable(true);
        ip_core.set_lsm_dc_block_enable(true);
        // Phase 10-prep: turn on the per-symbol LSM AGC. Direct
        // fixed-point port of SDRTrunk's P25P1DemodulatorLSM.java
        // AGC (L2 sqrt magnitude, `req_gain = 1.0 / mag`, 0.05
        // IIR lerp, asymmetric clamp at 500). See
        // maia-hdl/p25_hdl/lsm_agc.py and doc/changes/040.
        ip_core.set_lsm_agc_enable(true);
        // Read back lsm_control to confirm the bits actually stuck in the
        // register bank. If the readback disagrees with what we wrote we
        // have a register-bank wiring bug (rare; would surface as obvious
        // garbage in lsm_status / lsm_nid downstream).
        let (lsm_en_rb, lsm_dma_en_rb, lsm_dc_block_rb) =
            ip_core.lsm_control_readback();
        tracing::info!(
            "Control DDC: offset={nco_offset} Hz, dibit + iq + lsm ring DMA enabled \
             (lsm_control readback: lsm_enable={lsm_en_rb}, \
             lsm_dibit_dma_enable={lsm_dma_en_rb}, \
             lsm_dc_block_enable={lsm_dc_block_rb})"
        );
        if !lsm_en_rb || !lsm_dma_en_rb {
            tracing::error!(
                "lsm_control readback mismatch -- expected lsm_enable=true and \
                 lsm_dibit_dma_enable=true, got ({lsm_en_rb},{lsm_dma_en_rb}); \
                 HDL LSM chain WILL NOT be active"
            );
        }
        if !lsm_dc_block_rb {
            tracing::warn!(
                "lsm_dc_block_enable readback is false -- expected true; \
                 LSM front-end DC blocker is NOT active and the PLL \
                 acquisition transient will be 2-3 minutes instead of \
                 a few seconds (Phase 6G.1)"
            );
        }

        // Phase 7A.1: configure the traffic-channel DDC the same way the
        // control DDC is set up, then leave it disabled. The grant
        // follower task below flips the demod_enable bit and writes the
        // NCO frequency on demand whenever the control channel reports a
        // GroupVoiceChannelGrant.
        //
        // The traffic chain has been instantiated in HDL since Phase 4
        // (doc 007) but never driven from PS until now -- the existing
        // bitstream from Phase 6G.1 (08f7607) already contains it, so no
        // FPGA rebake is needed for 7A.1. The chain is C4FM-only at this
        // phase; Phase 7A.2 adds an LSM parallel chain on the traffic
        // side mirroring what Phase 6E.9 did on the control side.
        //
        // Initial NCO = 0 (centred on RX LO) so the chain has a defined
        // state before the first grant arrives. demod_enable starts at 0
        // to keep the dibit ring quiet until there's actually a call to
        // follow.
        ip_core.configure_traffic_ddc(0.0, args.sample_rate as f64)?;
        ip_core.set_traffic_ddc_enable(true);
        ip_core.set_traffic_demod_enable(false);
        tracing::info!(
            "Traffic DDC armed: NCO=0 Hz, ddc_enable=true, demod_enable=false \
             (will be flipped on by the grant follower on first GroupVoiceChannelGrant)"
        );

        // Phase 7A.2 + Phase 8B: arm the traffic-side LSM demod
        // chain without enabling it. The LSM master enable
        // (`traffic_lsm_enable`) is now toggled PER-CALL by the
        // follower retune path -- it's off at boot and between
        // calls, on only while a grant is being followed. This
        // closes the Phase 7 gap where the LSM chain was running
        // continuously against post-retune-transient dibits and
        // producing the phantom TDU_LC NID events that flooded
        // the traffic-side classifier. See doc/changes/038.
        //
        //   - traffic_lsm_enable: OFF at boot, flipped on in
        //       `retune_traffic_chain()` after the NCO write and
        //       the `traffic_lsm_reset` pulse.
        //   - traffic_lsm_dibit_dma_enable: armed level-high at
        //       boot so the ring DMA AW state machine is ready to
        //       stream as soon as the master enable opens.
        //   - traffic_lsm_dc_block_enable: on at boot so the
        //       leaky-integrator DC blocker has already converged
        //       by the time the first call arrives.
        ip_core.set_traffic_lsm_enable(false);
        ip_core.set_traffic_lsm_dibit_dma_enable(true);
        ip_core.set_traffic_lsm_dc_block_enable(true);
        // Phase 10-prep: arm the traffic-side per-symbol LSM AGC
        // at boot (same SDRTrunk-faithful port as the control
        // side above). The AGC stays armed across retunes; the
        // `traffic_lsm_reset` pulse in `retune_traffic_chain`
        // returns the gain register to GAIN_INIT (= 1.0) on every
        // retune, matching the clean-cold-start semantics.
        ip_core.set_traffic_lsm_agc_enable(true);
        let (tlsm_en_rb, tlsm_dma_en_rb, tlsm_dc_block_rb) =
            ip_core.traffic_lsm_control_readback();
        tracing::info!(
            "Traffic LSM chain armed: traffic_lsm_enable={tlsm_en_rb} \
             (Phase 8B: off until first retune), \
             traffic_lsm_dibit_dma_enable={tlsm_dma_en_rb}, \
             traffic_lsm_dc_block_enable={tlsm_dc_block_rb}"
        );
        if tlsm_en_rb || !tlsm_dma_en_rb {
            tracing::error!(
                "traffic_lsm_control readback mismatch -- expected enable=false \
                 (Phase 8B) and dibit_dma_enable=true, got \
                 ({tlsm_en_rb},{tlsm_dma_en_rb}); traffic-side LSM chain \
                 WILL NOT behave correctly on retune"
            );
        }
        if !tlsm_dc_block_rb {
            tracing::warn!(
                "traffic_lsm_dc_block_enable readback is false -- expected true; \
                 the LSM PLL acquisition transient on traffic-channel retunes \
                 will be longer than necessary"
            );
        }

        let ip_core = Arc::new(Mutex::new(ip_core));
        let ad9361 = Arc::new(ad9361);

        // 4. Get interrupt waiters before spawning handler
        let dibit_waiter = interrupt_handler.waiter_dibit_dma();
        // Phase 9 retirement: `iq_waiter` (waiter_iq_dma) used to
        // wake the Phase 6D software LSM pipeline. That pipeline
        // is gone, so we don't subscribe to the iq_dma interrupt
        // any more. The `InterruptHandler` still multiplexes the
        // raw IRQ line, it just doesn't have a PS consumer for
        // the iq_dma bit.
        let lsm_dibit_waiter = interrupt_handler.waiter_lsm_dibit_dma();
        // Phase 7A.1: traffic dibit DMA wakeups
        let traffic_dibit_waiter = interrupt_handler.waiter_traffic_dma();
        // Phase 7A.2 + 7C: traffic-side LSM dibit DMA wakeups. The
        // dibit reader task spawned below feeds the traffic_lsm_decoder
        // (which has the IMBE counter voice handler installed).
        let traffic_lsm_dibit_waiter = interrupt_handler.waiter_traffic_lsm_dibit_dma();

        // 5. Spawn interrupt handler
        let irq_stats_for_handler = irq_stats.clone();
        tokio::spawn(async move {
            if let Err(e) = interrupt_handler.run(irq_stats_for_handler).await {
                tracing::error!("interrupt handler error: {e}");
            }
        });

        // 6. Spawn dibit reader task
        let reader_core = ip_core.clone();
        let reader_decoder = decoder.clone();
        tokio::spawn(async move {
            tracing::info!("dibit reader task started");
            let mut wakeups: u64 = 0;
            let mut total_buffers: u64 = 0;
            let mut total_bytes: u64 = 0;
            // Cumulative dibit histogram across all reads
            let mut hist = [0u64; 4];
            loop {
                dibit_waiter.wait().await;
                wakeups += 1;
                let buffers = {
                    let mut core = reader_core.lock().await;
                    core.read_dibit_buffers()
                        .iter()
                        .map(|b| b.to_vec())
                        .collect::<Vec<_>>()
                };

                let mut wake_bytes = 0usize;
                let mut wake_dibits = 0usize;
                for buffer in &buffers {
                    wake_bytes += buffer.len();
                    let words: &[u64] = bytemuck_cast(buffer);
                    // Tally dibit histogram for this batch
                    for &word in words {
                        for i in 0..32 {
                            let d = ((word >> (i * 2)) & 0x03) as usize;
                            hist[d] += 1;
                            wake_dibits += 1;
                        }
                    }
                    let mut dec = reader_decoder.write().await;
                    for &word in words {
                        dec.process_dma_word(word);
                    }
                }
                total_buffers += buffers.len() as u64;
                total_bytes += wake_bytes as u64;

                if wakeups <= 5 || wakeups % 16 == 0 {
                    let total_dibits: u64 = hist.iter().sum();
                    let pct = |v: u64| -> f64 {
                        if total_dibits == 0 { 0.0 }
                        else { 100.0 * v as f64 / total_dibits as f64 }
                    };
                    tracing::info!(
                        target: "p25_reader",
                        "wake #{wakeups}: bufs={} bytes={} dibits={} \
                         (cum bufs={total_buffers} bytes={total_bytes}) \
                         hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}%",
                        buffers.len(), wake_bytes, wake_dibits,
                        pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                    );
                }
            }
        });

        // 6b. [RETIRED -- Phase 9 retirement, 2026-04-15]
        //
        // This slot used to be the Phase 6D LSM IQ reader task: it
        // woke on every iq_dma sub-buffer interrupt, ran a complete
        // pure-Rust LSM demod pipeline (decimate /2 -> LPF -> RRC ->
        // AGC+PLL+Gardner+slicer -> hard+soft sync correlators ->
        // BCH(63,16,11) FEC) on the raw 62.5 kSPS IQ samples,
        // dispatched soft-sync TSDU events into `iq_lsm_decoder`,
        // and updated the shared `LsmStats` that fed `/api/lsm`.
        //
        // After Phase 6E ported the full LSM demod into Amaranth
        // gateware (the production `LsmDemod` block running in
        // `lsm_ctrl_dom`) the software pipeline became pure dead
        // weight: the HDL chain produced the same TSBKs through
        // `lsm_decoder` using a fraction of the ARM CPU. Phase 9
        // formally retires it.
        //
        // The `iq_dma` HDL ring is still present in the bitstream
        // but is now disabled at boot (`set_iq_dma_enable(false)`)
        // so no data flows and no IRQs fire. It can be re-enabled
        // by a future phase for baseband capture to disk, a new
        // in-PL DSP block tapping post-DDC IQ, or reinstating the
        // software cross-check if a regression ever needs a raw-IQ
        // reference.
        //
        // See doc/changes/039 for the full retirement inventory.

        // 6c. Spawn HDL LSM dibit ring drain + TSBK decode task
        //     (Phase 6E.9/6E.10 bring-up).
        //
        //     The HDL LSM demod chain produces its own dibit stream via
        //     `lsm_dibit_dma`, parallel to the C4FM `dibit_dma` ring on
        //     the same control DDC output. We feed that stream into a
        //     SECOND, independent `ControlChannelDecoder` instance
        //     (`lsm_decoder`) which runs the identical Hunting ->
        //     ReadingNid -> ReadingDu -> trellis -> CRC -> TsbkMessage
        //     pipeline as the C4FM decoder above, just against a
        //     different dibit source. Both decoders land events on the
        //     same WebSocket broadcast channel so the dashboard sees a
        //     unified TSBK stream; the distinct trace targets
        //     (`p25_decoder` vs `p25_hdl_lsm_decoder`) let operators
        //     separate them in the server log.
        //
        //     **Why a separate instance instead of feeding into the
        //     existing decoder:** the two dibit streams come from two
        //     independent HDL demod chains with independent symbol
        //     timing loops. Frame sync alignment, NID boundaries, and
        //     TSU framing state are all specific to the stream they
        //     came from -- sharing state would corrupt either or both
        //     decoders. Two parallel instances is cheap (~300 bytes of
        //     state each on an ARM Cortex-A9) and gives us the
        //     cross-validation we want for bring-up: both decoders
        //     should emit IDENTICAL TSBK streams against the same RF
        //     capture, confirming the HDL LSM port is equivalent to
        //     the working Phase 2A C4FM path.
        //
        //     The existing Phase 6D in-PS Rust LSM pipeline keeps
        //     running in parallel (task 6b below) as a third
        //     independent sanity check. Retiring it is a Phase 6F
        //     decision after all three paths converge on hardware.
        let lsm_dibit_core = ip_core.clone();
        let lsm_dibit_decoder = lsm_decoder.clone();
        tokio::spawn(async move {
            tracing::info!(
                "HDL LSM dibit reader + TSBK decoder task started (Phase 6E)"
            );
            let mut wakeups: u64 = 0;
            let mut total_buffers: u64 = 0;
            let mut total_bytes: u64 = 0;
            let mut hist = [0u64; 4];
            loop {
                lsm_dibit_waiter.wait().await;
                wakeups += 1;
                let buffers = {
                    let mut core = lsm_dibit_core.lock().await;
                    core.read_lsm_dibit_buffers()
                        .iter()
                        .map(|b| b.to_vec())
                        .collect::<Vec<_>>()
                };

                let mut wake_bytes = 0usize;
                let mut wake_dibits = 0usize;
                for buffer in &buffers {
                    wake_bytes += buffer.len();
                    let words: &[u64] = bytemuck_cast(buffer);
                    // Histogram for bring-up diagnostics + feed into
                    // the LSM-side ControlChannelDecoder. The decoder
                    // does its own frame sync / NID / TSU / trellis /
                    // CRC chain internally, and emits TsbkMessage
                    // events on the shared WebSocket broadcast when
                    // a CRC-valid TSBK lands.
                    for &word in words {
                        for i in 0..32 {
                            let d = ((word >> (i * 2)) & 0x03) as usize;
                            hist[d] += 1;
                            wake_dibits += 1;
                        }
                    }
                    let mut dec = lsm_dibit_decoder.write().await;
                    for &word in words {
                        dec.process_dma_word(word);
                    }
                }
                total_buffers += buffers.len() as u64;
                total_bytes += wake_bytes as u64;

                if wakeups <= 5 || wakeups % 16 == 0 {
                    let total_dibits: u64 = hist.iter().sum();
                    let pct = |v: u64| -> f64 {
                        if total_dibits == 0 { 0.0 }
                        else { 100.0 * v as f64 / total_dibits as f64 }
                    };
                    // Snapshot the LSM decoder's cumulative sync /
                    // near-miss / recent-messages counters so we can
                    // see in the log whether the LSM dibit stream is
                    // actually producing frame sync hits (the single
                    // most important bring-up signal).
                    let (lsm_sync_hits, lsm_near, lsm_best, lsm_msg_count) = {
                        let d = lsm_dibit_decoder.read().await;
                        (
                            d.sync_hits(),
                            d.sync_near_misses(),
                            d.best_sync_distance(),
                            d.recent_messages.len(),
                        )
                    };
                    tracing::info!(
                        target: "p25_hdl_lsm",
                        "wake #{wakeups}: bufs={} bytes={} dibits={} \
                         (cum bufs={total_buffers} bytes={total_bytes}) \
                         hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}% \
                         | LSM decoder: sync_hits={lsm_sync_hits} \
                         near={lsm_near} best_dist={} recent_msgs={lsm_msg_count}",
                        buffers.len(), wake_bytes, wake_dibits,
                        pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                        if lsm_best == u32::MAX { 99 } else { lsm_best },
                    );
                }
            }
        });

        // Phase 7C: traffic-side LSM dibit reader + voice frame
        // decoder task. Mirrors the control-side LSM dibit reader
        // task immediately above (lines ~952-1032), feeding the
        // dibit stream into the new `traffic_lsm_decoder` instance
        // (which has the IMBE counter voice handler installed).
        //
        // Data flow:
        //   traffic_lsm_dibit_dma (DMA ring, ~3.4 sec sub-buffer fill)
        //     -> read_traffic_lsm_dibit_buffers (Vec<&[u8]>)
        //     -> bytemuck_cast (&[u64] of packed dibits)
        //     -> traffic_lsm_decoder.process_dma_word (Hunting ->
        //        ReadingNid -> ReadingDataUnit state machine)
        //     -> on_ldu1 / on_ldu2 / on_hdu / on_tdu / on_tdu_lc
        //        callbacks on the ImbeForwarder voice handler
        //     -> ImbeForwarder atomic counters incremented
        //     -> /api/traffic snapshot reads the atomics
        //
        // **Phase 7D will tap the same callback chain** to push raw
        // IMBE frames to the vocoder mpsc channel. Phase 7E will
        // wrap that with RTP audio output.
        //
        // The decoder runs the same state machine as the control
        // side -- it'll go through Hunting until a sync hit, decode
        // the NID, then dispatch by DUID. On a real call (Clay
        // County voice channel locked) we expect:
        //   - first event: HDU on call start
        //   - then 9-10x LDU1 / LDU2 alternation per second
        //   - final event: TDU or TDU_LC on call end
        let traffic_lsm_core = ip_core.clone();
        let traffic_lsm_decoder_task = traffic_lsm_decoder.clone();
        let traffic_reader_imbe = imbe_forwarder.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            tracing::info!(
                "Traffic LSM dibit reader + voice frame decoder task \
                 started (Phase 7C)"
            );
            let mut wakeups: u64 = 0;
            let mut total_buffers: u64 = 0;
            let mut total_bytes: u64 = 0;
            let mut hist = [0u64; 4];
            loop {
                traffic_lsm_dibit_waiter.wait().await;
                wakeups += 1;
                let buffers = {
                    let mut core = traffic_lsm_core.lock().await;
                    core.read_traffic_lsm_dibit_buffers()
                        .iter()
                        .map(|b| b.to_vec())
                        .collect::<Vec<_>>()
                };

                // Phase 7F.5 (2026-04-14) root-cause gate: if the
                // follower is Idle (current_talkgroup == 0), the
                // traffic channel isn't "open" -- but the HDL LSM
                // chain is still producing dibits from whatever
                // frequency the NCO is pointed at. Previously the
                // software framer happily processed every dibit,
                // extracted noise-shaped LDUs / TDU_LCs via sync
                // correlator false positives, dispatched them to
                // the WS activity feed + event log + vocoder, and
                // the dashboard read as "traffic channel active,
                // playing packets" when there was no real call.
                //
                // Drain the DMA ring (already done above by the
                // `read_traffic_lsm_dibit_buffers` call so the
                // hardware doesn't overflow) but DO NOT feed the
                // dibits to the framer. Counters and hist still
                // update so we can see raw dibit rate / histogram
                // via /api/traffic.stats even during Idle.
                let locked = traffic_reader_imbe
                    .current_talkgroup
                    .load(Ordering::Relaxed) != 0;

                let mut wake_bytes = 0usize;
                let mut wake_dibits = 0usize;
                for buffer in &buffers {
                    wake_bytes += buffer.len();
                    let words: &[u64] = bytemuck_cast(buffer);
                    for &word in words {
                        for i in 0..32 {
                            let d = ((word >> (i * 2)) & 0x03) as usize;
                            hist[d] += 1;
                            wake_dibits += 1;
                        }
                    }
                    if locked {
                        let mut dec = traffic_lsm_decoder_task.write().await;
                        for &word in words {
                            dec.process_dma_word(word);
                        }
                    }
                }
                total_buffers += buffers.len() as u64;
                total_bytes += wake_bytes as u64;

                if wakeups <= 5 || wakeups % 16 == 0 {
                    let total_dibits: u64 = hist.iter().sum();
                    let pct = |v: u64| -> f64 {
                        if total_dibits == 0 { 0.0 }
                        else { 100.0 * v as f64 / total_dibits as f64 }
                    };
                    let (sync_hits, msg_count, ldu1, ldu2, hdu, tdu, tdu_lc) = {
                        let d = traffic_lsm_decoder_task.read().await;
                        (
                            d.sync_hits(),
                            d.recent_messages.len(),
                            d.ldu1_count,
                            d.ldu2_count,
                            d.hdu_count,
                            d.tdu_count,
                            d.tdu_lc_count,
                        )
                    };
                    tracing::info!(
                        target: "p25_traffic_lsm",
                        "wake #{wakeups}: bufs={} bytes={} dibits={} \
                         (cum bufs={total_buffers} bytes={total_bytes}) \
                         hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}% \
                         | traffic_lsm decoder: sync_hits={sync_hits} \
                         hdu={hdu} ldu1={ldu1} ldu2={ldu2} tdu={tdu} \
                         tdu_lc={tdu_lc} recent_msgs={msg_count}",
                        buffers.len(), wake_bytes, wake_dibits,
                        pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                    );
                }
            }
        });

        // 6d. Spawn HDL LSM heartbeat / NID event poller (Phase 6E.9/6E.10).
        //
        //     Reads `lsm_status` + `lsm_debug` on EVERY tick at 60 Hz,
        //     not just when `nid_event` fires. This lets us see the
        //     state of the HDL LSM chain even when it isn't producing
        //     NID events, which is exactly the bring-up situation we
        //     hit on real RF.
        //
        //     Outputs per loop iteration:
        //     1. **NID event log** -- as before, fires only when
        //        `lsm_status.nid_event` (Rsticky) is high. Throttled to
        //        5 Hz on a busy site (~70 NIDs/sec) but always logs the
        //        first 10 events. Every event is ALSO captured into a
        //        32-deep ring buffer for the crash-transition dump
        //        described below.
        //     2. **Heartbeat log** -- fires every ~1 s regardless of
        //        whether NIDs are being decoded, dumping the windowed
        //        min/max of `pll_dbg` + `sample_point_dbg` + the lowest
        //        `sync_distance` seen in the window + iq_dma health
        //        + how many ticks of the window observed `bch_busy` /
        //        `in_nid_window` / `nid_event` / `dibit_overflow`.
        //     3. **Crash dump** -- the FIRST time `nid_evts == 0` in
        //        a heartbeat window after we've seen any healthy
        //        traffic, dump the full 32-deep NID ring buffer. This
        //        captures the exact NID events leading up to the
        //        transition from healthy to stalled, with no log
        //        throttling.
        //
        //     **Watchdog removed** (was Phase 6E.6 doc 023). Investigation
        //     after the 2026-04-10 CORDIC bake confirmed sdr_reset is
        //     fundamentally unsafe to pulse during operation -- it
        //     resets the entire `sync` clock domain, which interrupts
        //     in-flight AXI HP DMA writes, deadlocks the AXI HP slave
        //     in the PS DDR controller, and causes a hard kernel panic
        //     reboot. The previous "watchdog" was silently broken
        //     (CDC corruption swallowed its writes after the chain
        //     transitioned to the degraded state) -- if it had ever
        //     fired correctly, it would have crashed the board. The
        //     code is removed entirely; recovery from the degraded
        //     state requires either a power cycle or a future safer
        //     reset mechanism that drains in-flight AXI before
        //     asserting reset.
        let lsm_nid_core = ip_core.clone();
        let lsm_nid_runtime = hdl_lsm.clone();
        tokio::spawn(async move {
            tracing::info!("HDL LSM heartbeat + NID poller task started (Phase 6E)");
            // Stamp the start time as soon as we run.
            {
                let mut rt = lsm_nid_runtime.lock().await;
                rt.started_at = Some(std::time::Instant::now());
            }
            let mut tick = tokio::time::interval(
                std::time::Duration::from_millis(16),
            );
            tick.tick().await;

            // ── NID event tracking (cumulative) ─────────────────────
            let mut event_count: u64 = 0;
            let mut valid_count: u64 = 0;
            let mut last_drop_count: u16 = 0;
            let mut last_event_log = std::time::Instant::now();

            // ── NID event ring buffer for crash-transition dump ────
            // Captures the last 32 NID events with full state. Dumped
            // unconditionally on the first "nid_evts == 0" heartbeat
            // after at least one healthy heartbeat has been seen.
            const NID_RING_DEPTH: usize = 32;
            #[derive(Clone, Copy, Default)]
            struct NidRingEntry {
                seq: u64,
                t_ms_since_boot: u128,
                nac: u16,
                duid: u8,
                valid: bool,
                n_errors: u8,
                sync_distance: u8,
                drop_count: u16,
                pll_dbg: i16,
                sp_dbg: i16,
            }
            let mut nid_ring: [NidRingEntry; NID_RING_DEPTH] =
                [NidRingEntry::default(); NID_RING_DEPTH];
            let mut nid_ring_pos: usize = 0;
            let mut nid_ring_count: usize = 0;
            let mut crash_dump_armed = false;
            let mut crash_dump_fired = false;
            let task_start = std::time::Instant::now();

            // ── iq_dma drain-rate tracking for the heartbeat ───────
            let mut last_iq_next_addr: u32 = 0;
            let mut last_iq_last_buffer: u8 = 0xFF;
            let mut hb_iq_addr_advance: u64 = 0;
            let mut hb_iq_buffer_changes: u32 = 0;
            let mut hb_iq_overflow_ticks: u32 = 0;
            let mut last_iq_overflow_log = std::time::Instant::now();

            // ── Heartbeat windowed stats (reset on each emission) ──
            // Reset every ~1 s of polling = ~60 ticks at 16 ms.
            let mut hb_ticks: u32 = 0;
            let mut hb_pll_min: i16 = i16::MAX;
            let mut hb_pll_max: i16 = i16::MIN;
            let mut hb_sp_min: i16 = i16::MAX;
            let mut hb_sp_max: i16 = i16::MIN;
            // sync_distance is u8 0..47; track the BEST (lowest) hit in
            // the window. 99 is a "no observations yet" sentinel.
            let mut hb_sync_dist_best: u8 = 99;
            let mut hb_bch_busy_ticks: u32 = 0;
            let mut hb_in_window_ticks: u32 = 0;
            let mut hb_nid_event_ticks: u32 = 0;
            let mut hb_overflow_ticks: u32 = 0;
            // For NIDs that DID arrive in the window, what did we see?
            let mut hb_window_event_count: u32 = 0;
            let mut hb_window_valid_count: u32 = 0;

            let mut last_hb = std::time::Instant::now();

            loop {
                tick.tick().await;

                // EVERY tick: snapshot the full status + debug pair
                // PLUS the iq_dma health indicators. We read coherently
                // under the mutex so the heartbeat observes the same
                // instant the optional nid_event payload would
                // describe.
                let (
                    status,
                    nac,
                    duid,
                    drop_count,
                    pll_dbg,
                    sp_dbg,
                    iq_overflow,
                    iq_last_buffer,
                    iq_next_addr,
                ) = {
                    let core = lsm_nid_core.lock().await;
                    let s = core.lsm_status();
                    let (nac, duid) = core.lsm_nid();
                    let drop_count = core.lsm_drop_count();
                    let (pll_dbg, sp_dbg) = core.lsm_debug();
                    let iq_overflow = core.iq_overflow();
                    let iq_last_buffer = core.iq_last_buffer();
                    let iq_next_addr = core.iq_next_address();
                    (
                        s, nac, duid, drop_count, pll_dbg, sp_dbg,
                        iq_overflow, iq_last_buffer, iq_next_addr,
                    )
                };

                // ── iq_dma health bookkeeping for this tick ─────────
                // Track AW address advance + last_buffer rollover rate.
                // In healthy operation iq_next_addr advances ~5 KB per
                // tick (62.5kSPS * 4 bytes/sample / 60 Hz). If it
                // stops advancing or advances at <50 % of nominal, the
                // iq_dma write side has stalled.
                if last_iq_next_addr != 0 {
                    // Compute forward delta with wrap. Buffers in the
                    // ring are 4 KB and the ring wraps every 32 KB,
                    // so a single tick should never advance by more
                    // than 8 KB even at peak rate.
                    let delta = iq_next_addr.wrapping_sub(last_iq_next_addr);
                    // Filter out spurious huge backward jumps that
                    // would happen on a CDC read race.
                    if delta < 0x10000 {
                        hb_iq_addr_advance += delta as u64;
                    }
                }
                last_iq_next_addr = iq_next_addr;
                if iq_last_buffer != last_iq_last_buffer && last_iq_last_buffer != 0xFF {
                    hb_iq_buffer_changes += 1;
                }
                last_iq_last_buffer = iq_last_buffer;
                if iq_overflow {
                    hb_iq_overflow_ticks += 1;
                    // Throttle the per-tick warning to once per second
                    // so a stuck overflow doesn't drown the log.
                    if last_iq_overflow_log.elapsed()
                        >= std::time::Duration::from_secs(1)
                    {
                        last_iq_overflow_log = std::time::Instant::now();
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "iq_dma overflow latched (rate-limited; \
                             one warn per second of stuck state)"
                        );
                    }
                }

                // ── Fold this tick into the heartbeat window ────────
                hb_ticks += 1;
                if pll_dbg < hb_pll_min { hb_pll_min = pll_dbg; }
                if pll_dbg > hb_pll_max { hb_pll_max = pll_dbg; }
                if sp_dbg  < hb_sp_min  { hb_sp_min  = sp_dbg; }
                if sp_dbg  > hb_sp_max  { hb_sp_max  = sp_dbg; }
                if status.bch_busy      { hb_bch_busy_ticks += 1; }
                if status.in_nid_window { hb_in_window_ticks += 1; }
                if status.nid_event     { hb_nid_event_ticks += 1; }
                if status.dibit_overflow { hb_overflow_ticks += 1; }
                // Track the lowest sync_distance we ever see across
                // the window, regardless of whether nid_event fires.
                // The HDL latches sync_distance at the moment a sync
                // hit fires so it stays constant between events.
                // sync_distance == 0 sentinel after a perfect hit is
                // also legitimate, and the rsticky bits decay on read,
                // so we just take min over the raw reads.
                if status.sync_distance < hb_sync_dist_best {
                    hb_sync_dist_best = status.sync_distance;
                }

                // ── Per-tick: dibit overflow latch warning ──────────
                if status.dibit_overflow {
                    // Throttled to 1 Hz so a stuck overflow doesn't
                    // dominate the log (it used to fire every poll).
                    if last_iq_overflow_log.elapsed()
                        >= std::time::Duration::from_secs(1)
                    {
                        // (Reuse the same throttle as iq overflow --
                        // both signal the same upstream stall and
                        // we want one warn per second total.)
                    }
                    // Don't reset the throttle here; let the iq side
                    // manage it. We just count for the heartbeat.
                }

                // ── Phase 6F.2: live PL register snapshot to shared
                //    HdlLsmRuntime so /api/hdl_lsm can read it ───────
                {
                    let mut rt = lsm_nid_runtime.lock().await;
                    rt.last_tick_at = Some(std::time::Instant::now());
                    rt.pll_dbg = pll_dbg;
                    rt.sp_dbg = sp_dbg;
                    rt.sync_distance = status.sync_distance;
                    rt.bch_busy = status.bch_busy;
                    rt.in_nid_window = status.in_nid_window;
                    rt.dibit_overflow_latched = status.dibit_overflow;
                    rt.iq_overflow_latched = iq_overflow;
                    rt.last_nac = nac;
                    rt.last_duid = duid;
                    rt.last_drop_count = drop_count;
                    if status.dibit_overflow {
                        rt.dibit_overflow_ticks += 1;
                    }
                    if iq_overflow {
                        rt.iq_overflow_ticks += 1;
                    }
                }

                // ── Per-tick: NID event handling ────────────────────
                if status.nid_event {
                    event_count += 1;
                    hb_window_event_count += 1;
                    if status.nid_valid {
                        valid_count += 1;
                        hb_window_valid_count += 1;
                    }

                    // ALWAYS push the event into the ring buffer
                    // (regardless of throttling). This is what the
                    // crash dump reads.
                    let entry = NidRingEntry {
                        seq: event_count,
                        t_ms_since_boot: task_start.elapsed().as_millis(),
                        nac,
                        duid,
                        valid: status.nid_valid,
                        n_errors: status.n_errors,
                        sync_distance: status.sync_distance,
                        drop_count,
                        pll_dbg,
                        sp_dbg,
                    };
                    nid_ring[nid_ring_pos] = entry;
                    nid_ring_pos = (nid_ring_pos + 1) % NID_RING_DEPTH;
                    if nid_ring_count < NID_RING_DEPTH {
                        nid_ring_count += 1;
                    }
                    // Arm the crash-dump trigger as soon as we've
                    // seen any healthy traffic.
                    crash_dump_armed = true;

                    // ── Phase 6F.2: NID-event update to shared runtime
                    {
                        let mut rt = lsm_nid_runtime.lock().await;
                        rt.total_nid_events = event_count;
                        rt.valid_nid_events = valid_count;
                        rt.last_nid_at = Some(std::time::Instant::now());
                        rt.last_nid_valid = status.nid_valid;
                        rt.last_nid_n_errors = status.n_errors;
                        if status.nid_valid {
                            *rt.nac_hist.entry(nac).or_insert(0) += 1;
                        }
                        // Rebuild the ring as a chronological Vec for
                        // the dashboard.
                        let depth = nid_ring_count;
                        let start = if nid_ring_count < NID_RING_DEPTH {
                            0
                        } else {
                            nid_ring_pos
                        };
                        rt.nid_ring.clear();
                        for i in 0..depth {
                            let idx = (start + i) % NID_RING_DEPTH;
                            let e = nid_ring[idx];
                            rt.nid_ring.push(HdlNidEntry {
                                seq: e.seq,
                                t_ms_since_boot: e.t_ms_since_boot as u64,
                                nac: e.nac,
                                duid: e.duid,
                                valid: e.valid,
                                n_errors: e.n_errors,
                                sync_distance: e.sync_distance,
                                drop_count: e.drop_count,
                                pll_dbg: e.pll_dbg,
                                sp_dbg: e.sp_dbg,
                            });
                        }
                    }

                    if drop_count != last_drop_count {
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "lsm_drop_count bumped {} -> {} -- sync detector emitted a NID while BCH was busy",
                            last_drop_count, drop_count,
                        );
                        last_drop_count = drop_count;
                    }
                    // Throttle event logging to 5 Hz on a busy site
                    // (~70 NIDs/sec); always log the first 10.
                    let log_now = event_count <= 10
                        || last_event_log.elapsed() >= std::time::Duration::from_millis(200);
                    if log_now {
                        last_event_log = std::time::Instant::now();
                        tracing::info!(
                            target: "p25_hdl_lsm",
                            "NID event #{event_count}: nac=0x{:03X} duid={} \
                             valid={} n_errors={} sync_dist={} drop_count={} \
                             in_nid_window={} bch_busy={} pll={} sp={} \
                             (valid totals: {}/{})",
                            nac, duid, status.nid_valid, status.n_errors,
                            status.sync_distance, drop_count,
                            status.in_nid_window, status.bch_busy,
                            pll_dbg, sp_dbg,
                            valid_count, event_count,
                        );
                    }
                }

                // ── Heartbeat log (every ~1 s of polling) ───────────
                if last_hb.elapsed() >= std::time::Duration::from_secs(1) {
                    let pll_range = if hb_pll_min == i16::MAX {
                        "[no samples]".to_string()
                    } else {
                        format!("[{},{}]", hb_pll_min, hb_pll_max)
                    };
                    let sp_range = if hb_sp_min == i16::MAX {
                        "[no samples]".to_string()
                    } else {
                        format!("[{},{}]", hb_sp_min, hb_sp_max)
                    };
                    let best_str = if hb_sync_dist_best == 99 {
                        "n/a".to_string()
                    } else {
                        hb_sync_dist_best.to_string()
                    };
                    // iq_dma health: KB/s and buffer rollover rate.
                    let iq_kbps = hb_iq_addr_advance / 1024;
                    tracing::info!(
                        target: "p25_hdl_lsm",
                        "HB {hb_ticks}t: pll{pll_range} sp{sp_range} \
                         best_sync_dist={best_str} \
                         bch_busy={hb_bch_busy_ticks} \
                         in_window={hb_in_window_ticks} \
                         nid_evts={hb_nid_event_ticks} \
                         dibit_overflow={hb_overflow_ticks} \
                         iq_overflow={hb_iq_overflow_ticks} \
                         iq_kbps={iq_kbps} \
                         iq_buf_rolls={hb_iq_buffer_changes} \
                         (window NIDs: {hb_window_valid_count}/{hb_window_event_count} valid; \
                         cum NIDs: {valid_count}/{event_count})"
                    );

                    // ── Crash-transition NID ring dump ──────────────
                    //
                    // The MOMENT this is the first heartbeat with
                    // zero NID events after we've seen at least one
                    // healthy heartbeat, dump the full ring buffer.
                    // This captures up to 32 NID events with full
                    // pll/sp/sync_dist/n_errors/drop_count state, no
                    // throttling. Compare entries N..N+5 (the
                    // tail) for the moment things went wrong.
                    //
                    // Fires exactly once per boot. Subsequent stuck
                    // heartbeats just emit the regular HB line.
                    if crash_dump_armed
                        && !crash_dump_fired
                        && hb_window_event_count == 0
                    {
                        crash_dump_fired = true;
                        let depth = nid_ring_count;
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "CRASH TRANSITION: HDL LSM chain produced 0 \
                             NID events in the past 1s window after a \
                             healthy run -- dumping the last {} NID \
                             events from the ring buffer:",
                            depth,
                        );
                        // Walk the ring in chronological order
                        // (oldest -> newest) so the log reads
                        // top-to-bottom in time order.
                        let start = if nid_ring_count < NID_RING_DEPTH {
                            0
                        } else {
                            nid_ring_pos
                        };
                        for i in 0..depth {
                            let idx = (start + i) % NID_RING_DEPTH;
                            let e = nid_ring[idx];
                            tracing::warn!(
                                target: "p25_hdl_lsm",
                                "  ring[{:2}] t={:>6}ms #{:>5} \
                                 nac=0x{:03X} duid={} valid={:>5} \
                                 n_errors={:>2} sync_dist={:>2} \
                                 drop_count={:>5} pll={:>6} sp={:>6}",
                                i,
                                e.t_ms_since_boot,
                                e.seq,
                                e.nac,
                                e.duid,
                                e.valid,
                                e.n_errors,
                                e.sync_distance,
                                e.drop_count,
                                e.pll_dbg,
                                e.sp_dbg,
                            );
                        }
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "CRASH TRANSITION: end of ring dump. Chain \
                             will likely remain stuck until power \
                             cycle (sdr_reset is unsafe to use during \
                             operation -- see doc/changes/024)."
                        );
                    }

                    // ── Phase 6F.2: snapshot completed window into the
                    //    shared runtime BEFORE we reset accumulators ──
                    {
                        let mut rt = lsm_nid_runtime.lock().await;
                        rt.hb_pll_min = if hb_pll_min == i16::MAX { 0 } else { hb_pll_min };
                        rt.hb_pll_max = if hb_pll_max == i16::MIN { 0 } else { hb_pll_max };
                        rt.hb_sp_min = if hb_sp_min == i16::MAX { 0 } else { hb_sp_min };
                        rt.hb_sp_max = if hb_sp_max == i16::MIN { 0 } else { hb_sp_max };
                        rt.hb_sync_dist_best = hb_sync_dist_best;
                        rt.hb_bch_busy_ticks = hb_bch_busy_ticks;
                        rt.hb_in_window_ticks = hb_in_window_ticks;
                        rt.hb_nid_event_ticks = hb_nid_event_ticks;
                        rt.hb_dibit_overflow_ticks = hb_overflow_ticks;
                        rt.hb_iq_overflow_ticks = hb_iq_overflow_ticks;
                        rt.hb_iq_kbps = iq_kbps;
                        rt.hb_iq_buf_rolls = hb_iq_buffer_changes;
                        rt.hb_window_valid_count = hb_window_valid_count;
                        rt.hb_window_event_count = hb_window_event_count;
                    }

                    // Reset windowed accumulators for the next second.
                    hb_ticks = 0;
                    hb_pll_min = i16::MAX;
                    hb_pll_max = i16::MIN;
                    hb_sp_min = i16::MAX;
                    hb_sp_max = i16::MIN;
                    hb_sync_dist_best = 99;
                    hb_bch_busy_ticks = 0;
                    hb_in_window_ticks = 0;
                    hb_nid_event_ticks = 0;
                    hb_overflow_ticks = 0;
                    hb_iq_addr_advance = 0;
                    hb_iq_buffer_changes = 0;
                    hb_iq_overflow_ticks = 0;
                    hb_window_event_count = 0;
                    hb_window_valid_count = 0;
                    last_hb = std::time::Instant::now();
                }
            }
        });

        // 7. Spawn periodic stats task — polls FPGA registers every 2s
        let stats_core = ip_core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            tick.tick().await; // discard first immediate tick
            loop {
                tick.tick().await;
                let core = stats_core.lock().await;
                tracing::info!(
                    target: "p25_stats",
                    "regs: dibit_count={} overflow={} last_buffer={} next_addr=0x{:08X} \
                     lsm_last_buffer={} lsm_next_addr=0x{:08X}",
                    core.dibit_count(),
                    core.demod_overflow(),
                    core.dibit_last_buffer(),
                    core.dibit_next_address(),
                    core.lsm_dibit_last_buffer(),
                    core.lsm_dibit_next_address(),
                );
            }
        });

        // Phase 7A.1 (a): traffic dibit reader task. Wakes on every
        // traffic_dma sub-buffer interrupt, drains the ring via
        // `read_traffic_buffers()`, counts dibits + maintains a per-dibit
        // histogram, and updates the shared `TrafficStats`. Does NOT
        // feed a decoder -- the C4FM chain produces garbage on the LSM
        // Clay County voice channels we are validating against. The
        // histogram alone is enough to confirm "the chain is alive": a
        // dead chain produces all-zero dibits, a live chain produces an
        // even-ish spread across {0,1,2,3} (LSM through a C4FM slicer
        // looks essentially random).
        //
        // Mirrors the control-channel dibit reader at line ~358 above
        // but against the traffic_dma ring + traffic stats sink. Bumps
        // `note_activity()` on the TrafficManager whenever new bytes
        // arrive so the call_timeout_ms inactivity detector resets.
        let traffic_reader_core = ip_core.clone();
        let traffic_reader_stats = traffic_stats.clone();
        let traffic_reader_mgr = traffic_manager.clone();
        tokio::spawn(async move {
            tracing::info!("traffic dibit reader task started (Phase 7A.1)");
            loop {
                traffic_dibit_waiter.wait().await;
                // Snapshot under the lock, then drop it before CPU work.
                let buffers: Vec<Vec<u8>> = {
                    let mut core = traffic_reader_core.lock().await;
                    core.read_traffic_buffers()
                        .iter()
                        .map(|b| b.to_vec())
                        .collect()
                };
                if buffers.is_empty() {
                    continue;
                }

                let mut wake_bytes = 0usize;
                let mut wake_dibits = 0usize;
                let mut wake_hist = [0u64; 4];
                for buffer in &buffers {
                    wake_bytes += buffer.len();
                    let words: &[u64] = bytemuck_cast(buffer);
                    for &word in words {
                        for i in 0..32 {
                            let d = ((word >> (i * 2)) & 0x03) as usize;
                            wake_hist[d] += 1;
                            wake_dibits += 1;
                        }
                    }
                }

                {
                    let mut s = traffic_reader_stats.lock().await;
                    let now = std::time::Instant::now();
                    if s.started_at.is_none() {
                        s.started_at = Some(now);
                    }
                    s.last_at = Some(now);
                    s.wakeups += 1;
                    s.total_buffers += buffers.len() as u64;
                    s.total_bytes += wake_bytes as u64;
                    s.total_dibits += wake_dibits as u64;
                    for i in 0..4 {
                        s.dibit_hist[i] += wake_hist[i];
                    }
                    // Use plain modulo, not `is_multiple_of` -- the
                    // latter is unstable (`int_roundings` feature gate)
                    // and the Tezuka Buildroot Rust toolchain is older
                    // stable.
                    let log_now = s.wakeups <= 5 || s.wakeups % 64 == 0;
                    if log_now {
                        let total_d: u64 = s.dibit_hist.iter().sum();
                        let pct = |v: u64| -> f64 {
                            if total_d == 0 {
                                0.0
                            } else {
                                100.0 * v as f64 / total_d as f64
                            }
                        };
                        tracing::info!(
                            target: "p25_traffic",
                            "wake #{}: bufs={} bytes={} dibits={} \
                             (cum bufs={} bytes={} dibits={}) \
                             hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}%",
                            s.wakeups, buffers.len(), wake_bytes, wake_dibits,
                            s.total_buffers, s.total_bytes, s.total_dibits,
                            pct(s.dibit_hist[0]), pct(s.dibit_hist[1]),
                            pct(s.dibit_hist[2]), pct(s.dibit_hist[3]),
                        );
                    }
                }

                // Pet the TrafficManager so its 3-second
                // call-inactivity timeout doesn't fire while real bytes
                // are still arriving.
                {
                    let mut mgr = traffic_reader_mgr.lock().await;
                    mgr.note_activity();
                }
            }
        });

        // Phase 7A.1 (b): traffic grant follower task. Polls the
        // canonical LSM control-channel decoder's `grants` HashMap at
        // 50 ms cadence (well within the P25 ~200 ms grant-follow
        // budget), picks the most recent grant with a known frequency,
        // and forwards it to the TrafficManager. On a state change to
        // a new channel, retunes the traffic DDC and asserts demod_enable.
        // On the TrafficManager going Idle (3 s of no activity), drops
        // demod_enable to quiet the dibit ring.
        //
        // POLLING CHOICE (vs. typed broadcast events): the existing
        // `event_tx` is `broadcast::Sender<String>` -- it carries
        // pre-formatted strings, not enums. Subscribing and parsing
        // strings is brittle. The two clean alternatives -- adding a
        // parallel `Sender<GrantEvent>` channel or a callback hook on
        // the decoder -- both touch every grant dispatch site in
        // control_channel.rs. For Phase 7A.1 ("wire it up, prove the
        // path") polling is sufficient: 50 ms gives <100 ms total
        // latency, which is half the P25 budget. Phase 7B will replace
        // this with a typed event channel when modulation auto-detect
        // and per-grant lifecycle hooks force tighter coupling.
        //
        // We poll `lsm_decoder` (not `decoder` or `iq_lsm_decoder`)
        // because per the AppState doc comment that's the canonical
        // source for the dashboard's Active Grants panel.
        let follower_lsm_decoder = lsm_decoder.clone();
        let follower_mgr = traffic_manager.clone();
        let follower_core = ip_core.clone();
        let follower_sample_rate = args.sample_rate as f64;
        let follower_rx_lo = args.rx_lo as i64;
        let follower_enabled = traffic_follower_enabled.clone();
        let follower_imbe = imbe_forwarder.clone();
        let follower_monitor = monitor_list.clone();
        let follower_event_log = event_log.clone();
        let follower_traffic_decoder = traffic_lsm_decoder.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            tracing::info!(
                "traffic grant follower task started (Phase 7B, \
                 event-driven via mpsc + 200 ms timeout tick)"
            );
            let mut timeout_tick =
                tokio::time::interval(std::time::Duration::from_millis(200));
            timeout_tick.tick().await; // discard immediate first tick

            // Helper closure: process a grant event. Returns true if
            // a retune was performed.
            //
            // Sticky-lock policy (from SDRTrunk PR #2010):
            // - If locked on a TG, only accept grants for that TG.
            // - If Idle, accept according to monitor list priority
            //   (or newest if monitor list is empty).
            let handle_grant_event =
                |g: &p25::events::GrantEvent,
                 mgr: &mut p25::traffic_manager::TrafficManager,
                 imbe: &ImbeForwarder| -> bool
            {
                let freq_hz = match g.frequency_hz {
                    Some(f) => f,
                    None => return false,
                };
                let retune = mgr.handle_grant(g.channel, g.talkgroup, freq_hz);
                imbe.current_talkgroup.store(g.talkgroup.0, Ordering::Relaxed);

                // Determine encryption: check the grant flag, then
                // fall back to TG history (remembers TGs that were
                // ever seen encrypted).
                let is_enc = if g.encrypted {
                    // Record this TG as encrypted for future lookups
                    if let Ok(mut hist) = imbe.encrypted_tg_history.lock() {
                        hist.insert(g.talkgroup.0);
                    }
                    true
                } else {
                    // Grant doesn't say encrypted -- check history
                    imbe.encrypted_tg_history.lock()
                        .map(|h| h.contains(&g.talkgroup.0))
                        .unwrap_or(false)
                };

                if retune {
                    // New call: set encryption and reset vocoder
                    imbe.call_encrypted.store(is_enc, Ordering::Relaxed);
                    imbe.vocoder_reset_pending.store(true, Ordering::Relaxed);
                } else if is_enc {
                    // Sticky-true within a call
                    imbe.call_encrypted.store(true, Ordering::Relaxed);
                }
                // Note: we deliberately do NOT set call_encrypted=false
                // on a grant refresh where g.encrypted==false. The flag
                // is cleared only on Idle transition.
                retune
            };

            // Phase 9.1 (2026-04-15): the activity log is
            // DELIBERATELY NOT deduped. Every `P25Event::Grant`
            // arrival gets its own log line -- even two
            // back-to-back grants on the same (TG, channel, freq)
            // that were packed into the same 3-TSBK TSDU by the
            // trunking system. The correct-handling invariant
            // lives one layer down in `handle_grant_event` ->
            // `TrafficManager::handle_grant`: the `same_tg_same_freq`
            // branch at traffic_manager.rs:258 short-circuits with
            // `return false` (no retune fires, no second state
            // transition, no second `retune_traffic_chain()` call)
            // whenever the new grant matches the current lock.
            // Auto-promote from Acquiring to Active happens on
            // that same branch. So the dashboard sees all the
            // real TSBK arrivals, the follower fires exactly one
            // retune per real channel change, and no work is
            // duplicated even if two grants land in the same
            // millisecond.

            loop {
                tokio::select! {
                    event = grant_event_rx.recv() => {
                        let event = match event {
                            Some(e) => e,
                            None => break, // channel closed
                        };

                        if !follower_enabled.load(Ordering::Relaxed) {
                            continue;
                        }

                        match event {
                            p25::events::P25Event::Grant(g) => {
                                use crate::event_log::LogCategory;
                                let freq_mhz = g.frequency_hz
                                    .map(|f| f as f64 / 1e6)
                                    .unwrap_or(0.0);

                                // Phase 7F.4 (2026-04-14): eager history
                                // populate. Any grant with encrypted=true
                                // adds the TG to the persistent history
                                // RIGHT HERE, before any gate check. The
                                // previous flash only populated history
                                // inside the reject path, so a site that
                                // sometimes-sets / sometimes-doesn't set
                                // service_options would let us retune to
                                // the same encrypted TG 3+ times before
                                // the history eventually caught up. This
                                // way, the first encrypted=true
                                // observation for ANY TG permanently
                                // blocks all subsequent grants for it --
                                // even ones that arrive missing the
                                // service-options flag on their next
                                // transmission.
                                if g.encrypted {
                                    if let Ok(mut hist) =
                                        follower_imbe
                                            .encrypted_tg_history
                                            .lock()
                                    {
                                        hist.insert(g.talkgroup.0);
                                    }
                                }

                                // Raw grant receipt (before any filter).
                                // Logged unconditionally -- two
                                // simultaneous grants for the same
                                // TG/channel/freq are real TSBK
                                // arrivals and both belong in the
                                // activity feed. Double-retune
                                // protection lives in
                                // TrafficManager::handle_grant's
                                // `same_tg_same_freq` branch (see
                                // traffic_manager.rs:258).
                                follower_event_log.push(
                                    LogCategory::Grant,
                                    format!(
                                        "grant TG={} ch={} {:.4} MHz{}",
                                        g.talkgroup.0,
                                        g.channel.0,
                                        freq_mhz,
                                        if g.encrypted { " [ENC]" } else { "" },
                                    ),
                                    serde_json::json!({
                                        "tg":        g.talkgroup.0,
                                        "channel":   g.channel.0,
                                        "frequency": g.frequency_hz,
                                        "src":       g.source.map(|r| r.0),
                                        "encrypted": g.encrypted,
                                        "emergency": g.emergency,
                                    }),
                                );

                                // Monitor list gate
                                let dominated = {
                                    let monitor = follower_monitor.read().await;
                                    if monitor.is_empty() {
                                        true // accept all
                                    } else {
                                        monitor.contains(g.talkgroup.0)
                                    }
                                };
                                if !dominated {
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "reject: TG={} not in monitor list",
                                            g.talkgroup.0,
                                        ),
                                        serde_json::json!({
                                            "tg":     g.talkgroup.0,
                                            "reason": "monitor_list",
                                        }),
                                    );
                                    continue;
                                }

                                // Sticky-lock check
                                let mut mgr = follower_mgr.lock().await;
                                let locked_tg = mgr.current_talkgroup();
                                match locked_tg {
                                    Some(tg) if tg.0 != g.talkgroup.0 => {
                                        follower_event_log.push(
                                            LogCategory::Traffic,
                                            format!(
                                                "reject: TG={} (sticky-locked on TG={})",
                                                g.talkgroup.0, tg.0,
                                            ),
                                            serde_json::json!({
                                                "tg":        g.talkgroup.0,
                                                "locked_tg": tg.0,
                                                "reason":    "sticky_lock",
                                            }),
                                        );
                                        continue;
                                    }
                                    _ => {}
                                }

                                // Encrypted-grant gate (Phase 7F.2,
                                // 2026-04-14). Runs on EVERY grant
                                // -- not just new locks -- because
                                // the real-world failure mode is a
                                // TG whose first grant has no service
                                // options (encrypted=false), passes
                                // through, locks the follower, then
                                // the next `GrantUpdate` arrives with
                                // encrypted=true. Previous gate
                                // (locked_tg.is_none()-only) let that
                                // path through the sticky-same-TG
                                // branch and the call ran for 2 s
                                // before call_timeout_ms released the
                                // lock -- producing ~72 encrypted
                                // vocoder frames before the tear-down.
                                //
                                // New behaviour: encryption detected,
                                // always reject the grant AND force
                                // the manager to Idle + drop
                                // demod_enable so the decoder stops
                                // feeding encrypted LDUs forward.
                                // History is populated eagerly on the
                                // first encrypted observation for
                                // each TG so subsequent grants short-
                                // circuit immediately.
                                let tg_known_enc = follower_imbe
                                    .encrypted_tg_history
                                    .lock()
                                    .map(|h| h.contains(&g.talkgroup.0))
                                    .unwrap_or(false);
                                if g.encrypted || tg_known_enc {
                                    if g.encrypted {
                                        if let Ok(mut hist) =
                                            follower_imbe
                                                .encrypted_tg_history
                                                .lock()
                                        {
                                            hist.insert(g.talkgroup.0);
                                        }
                                    }
                                    mgr.grants_rejected_encrypted += 1;

                                    // If the encrypted TG happens to
                                    // be the one we're currently
                                    // locked on, tear down the lock
                                    // synchronously. Without this
                                    // the sticky 2 s timeout would
                                    // hold the slot until the call
                                    // naturally ends.
                                    let was_locked = locked_tg
                                        .map(|t| t.0 == g.talkgroup.0)
                                        .unwrap_or(false);
                                    if was_locked {
                                        mgr.force_idle();
                                        drop(mgr);
                                        // Zero current_talkgroup so
                                        // the vocoder task's TG-change
                                        // auto-flush fires and closes
                                        // the call summary.
                                        follower_imbe.current_talkgroup
                                            .store(0, Ordering::Relaxed);
                                        // DO NOT clear call_encrypted
                                        // here. Buffered LDU dibits
                                        // from the previous channel
                                        // are still in flight in the
                                        // DMA ring + mpsc channel;
                                        // clearing the flag would
                                        // cause the vocoder to DECODE
                                        // those encrypted LDUs as
                                        // clear, producing garbled
                                        // output. Leaving it `true`
                                        // keeps the skip path active
                                        // until the next valid retune
                                        // (which unconditionally
                                        // writes call_encrypted =
                                        // new_grant.is_enc).
                                        #[cfg(target_os = "linux")]
                                        {
                                            let core = follower_core
                                                .lock().await;
                                            // Phase 8B: quiesce both
                                            // the LSM and C4FM chains
                                            // on encrypted tear-down
                                            // so the traffic LSM demod
                                            // stops producing phantom
                                            // NID events during the
                                            // gap until the next
                                            // grant.
                                            core.pause_traffic_chain();
                                        }
                                        // Reset the traffic framer --
                                        // it's mid-frame on encrypted
                                        // data and will carry bogus
                                        // state into whatever lock we
                                        // pick up next.
                                        {
                                            let mut dec = follower_traffic_decoder
                                                .write().await;
                                            dec.reset_framer_state();
                                        }
                                    }

                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "reject TG={} encrypted{}",
                                            g.talkgroup.0,
                                            if was_locked {
                                                " (tore down active lock)"
                                            } else { "" },
                                        ),
                                        serde_json::json!({
                                            "tg":         g.talkgroup.0,
                                            "enc_flag":   g.encrypted,
                                            "in_history": tg_known_enc,
                                            "was_locked": was_locked,
                                            "reason":     "encrypted",
                                        }),
                                    );
                                    continue;
                                }

                                let pre_state = mgr.state_label();
                                let retune = handle_grant_event(
                                    &g, &mut mgr, &follower_imbe
                                );
                                let post_state = mgr.state_label();
                                drop(mgr);

                                if pre_state != post_state {
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "state {} -> {} TG={}",
                                            pre_state, post_state, g.talkgroup.0,
                                        ),
                                        serde_json::json!({
                                            "from": pre_state,
                                            "to":   post_state,
                                            "tg":   g.talkgroup.0,
                                        }),
                                    );
                                }

                                if retune {
                                    let freq_hz = g.frequency_hz.unwrap();
                                    let offset_hz = freq_hz as i64 - follower_rx_lo;

                                    // Phase 7F.1 fix: reset the
                                    // traffic-side decoder framer
                                    // *before* the DDC retune so
                                    // dibits arriving from the new
                                    // frequency don't get consumed
                                    // while the framer is mid-state
                                    // on stale data. Preserves
                                    // cumulative counters.
                                    {
                                        let mut dec = follower_traffic_decoder
                                            .write().await;
                                        dec.reset_framer_state();
                                    }

                                    let core = follower_core.lock().await;
                                    // Phase 8B: atomic
                                    // freeze-reset-thaw through the
                                    // HDL reset plumbing added in
                                    // Phase 8A. `retune_traffic_chain`
                                    // disables both the LSM and C4FM
                                    // chains, writes the new DDC
                                    // frequency, pulses
                                    // `traffic_lsm_reset` (clearing
                                    // the PLL accumulator and all
                                    // upstream state), then re-enables
                                    // both chains. The post-retune PLL
                                    // starts from 0 and converges in
                                    // ~50-100 ms instead of carrying
                                    // stale phase from the previous
                                    // carrier. See doc/changes/038.
                                    match core.retune_traffic_chain(
                                        offset_hz as f64,
                                        follower_sample_rate,
                                    ) {
                                        Ok(()) => {
                                            tracing::info!(
                                                target: "p25_traffic",
                                                "retune: TG={} channel={:?} \
                                                 freq={} Hz offset={:+} Hz \
                                                 (LSM freeze-reset-thaw, framer reset)",
                                                g.talkgroup.0, g.channel,
                                                freq_hz, offset_hz,
                                            );
                                            follower_event_log.push(
                                                LogCategory::Traffic,
                                                format!(
                                                    "retune TG={} -> {:.4} MHz (offset {:+} Hz)",
                                                    g.talkgroup.0,
                                                    freq_hz as f64 / 1e6,
                                                    offset_hz,
                                                ),
                                                serde_json::json!({
                                                    "tg":          g.talkgroup.0,
                                                    "channel":     g.channel.0,
                                                    "frequency":   freq_hz,
                                                    "offset_hz":   offset_hz,
                                                    "framer_reset": true,
                                                    "lsm_reset":   true,
                                                }),
                                            );
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                target: "p25_traffic",
                                                "traffic DDC retune failed: \
                                                 TG={} freq={} Hz \
                                                 offset={:+} Hz: {}",
                                                g.talkgroup.0, freq_hz,
                                                offset_hz, e,
                                            );
                                            follower_event_log.push(
                                                LogCategory::Traffic,
                                                format!(
                                                    "retune FAILED TG={}: {}",
                                                    g.talkgroup.0, e,
                                                ),
                                                serde_json::json!({
                                                    "tg":     g.talkgroup.0,
                                                    "error":  e.to_string(),
                                                }),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ = timeout_tick.tick() => {
                        if !follower_enabled.load(Ordering::Relaxed) {
                            continue;
                        }
                        let mut mgr = follower_mgr.lock().await;
                        let pre_timeout_tg = mgr.current_talkgroup();
                        if mgr.check_timeouts() {
                            drop(mgr);
                            let core = follower_core.lock().await;
                            // Phase 8B: pause both chains (LSM +
                            // C4FM) between calls so the traffic
                            // LSM demod is quiescent during Idle --
                            // no phantom NID events, no drift in
                            // the PLL accumulator against noise.
                            core.pause_traffic_chain();
                            if let Some(tg) = pre_timeout_tg {
                                let mut dec =
                                    follower_lsm_decoder.write().await;
                                dec.grants.retain(
                                    |_, g| g.talkgroup.0 != tg.0
                                );
                                tracing::info!(
                                    target: "p25_traffic",
                                    "traffic Idle (timeout) -- removed \
                                     TG {} from grant store, \
                                     demod_enable=off",
                                    tg.0,
                                );
                                follower_event_log.push(
                                    crate::event_log::LogCategory::Traffic,
                                    format!(
                                        "state -> Idle (timeout) TG={}",
                                        tg.0,
                                    ),
                                    serde_json::json!({
                                        "tg":       tg.0,
                                        "to":       "Idle",
                                        "reason":   "call_timeout",
                                    }),
                                );
                            } else {
                                tracing::info!(
                                    target: "p25_traffic",
                                    "traffic Idle (timeout) -- \
                                     demod_enable=off"
                                );
                            }
                            follower_imbe.call_encrypted.store(
                                false, Ordering::Relaxed,
                            );
                            follower_imbe.current_talkgroup.store(
                                0, Ordering::Relaxed,
                            );
                        }
                    }
                }
            }
            tracing::warn!("grant follower task exiting (channel closed)");
        });

        // Phase 7A.2 (c): traffic LSM heartbeat task. Polls
        // `traffic_lsm_status` at 16 ms cadence (matches the typical
        // NID arrival rate of one per ~14 ms on a P25 voice channel:
        // HDU + LDU1 + LDU2 + LDU1 + LDU2 + ... + TDU). On every
        // `nid_event=true` read, dispatches the latched NAC + DUID
        // to the appropriate TrafficManager handler:
        //
        //   DUID 0x0  HDU         -> hdu_received(now, nac)
        //   DUID 0x3  TDU         -> tdu_received(now, nac, false)
        //   DUID 0xF  TDU_LC      -> tdu_received(now, nac, true)
        //   DUID 0x5  LDU1        -> ldu_received(now, nac, false)
        //   DUID 0xA  LDU2        -> ldu_received(now, nac, true)
        //
        // The 16 ms cadence is fine-grained enough that we won't
        // miss back-to-back NIDs (which arrive ~14 ms apart on a
        // sustained voice channel). Phase 7A.1 polled
        // `lsm_decoder.grants` at 50 ms; this is faster because each
        // missed NID event is a strict information loss (the Rsticky
        // bit gets cleared on the next read but the latched fields
        // are overwritten).
        //
        // Note: this is structured almost identically to the existing
        // HDL LSM heartbeat task in p25-httpd that polls
        // `lsm_status` for the control side. We could refactor both
        // into a shared helper later -- for Phase 7A.2 the duplicated
        // code is acceptable because the dispatch handlers differ
        // (TrafficManager vs ControlChannelDecoder).
        let traffic_lsm_core = ip_core.clone();
        let traffic_lsm_mgr = traffic_manager.clone();
        let traffic_lsm_stats = traffic_stats.clone();
        let traffic_event_tx = event_tx.clone();
        let traffic_event_log = event_log.clone();
        let traffic_heartbeat_imbe = imbe_forwarder.clone();
        tokio::spawn(async move {
            tracing::info!(
                "traffic LSM heartbeat task started (Phase 7A.2, polling \
                 traffic_lsm_status @ 16 ms)"
            );
            let mut tick =
                tokio::time::interval(std::time::Duration::from_millis(16));
            tick.tick().await; // discard immediate first tick
            // Track cumulative NID counters locally for periodic
            // logging (TrafficManager already tracks hdus_seen /
            // ldus_seen / tdus_seen).
            let mut total_polls: u64 = 0;
            let mut nid_events: u64 = 0;
            loop {
                tick.tick().await;
                total_polls += 1;

                // Read the status, NAC/DUID, and debug taps under one
                // brief lock acquisition. Per the lsm_status() doc
                // comment in fpga.rs, the snapshot + follow-up
                // traffic_lsm_nid() form a coherent per-NID picture.
                let (status, nac, duid) = {
                    let core = traffic_lsm_core.lock().await;
                    let s = core.traffic_lsm_status();
                    let (n, d) = core.traffic_lsm_nid();
                    (s, n, d)
                };

                if !status.nid_event {
                    continue;
                }
                nid_events += 1;

                // Phase 7F.5 (2026-04-14) root-cause gate: if the
                // follower is Idle, skip the entire NID dispatch.
                // The HDL LSM chain keeps producing NID events on
                // residual dibits between calls, and before this
                // gate the heartbeat was calling mgr.hdu_received
                // / ldu_received / tdu_received / broadcasting WS
                // activity / pushing event_log entries for every
                // noise-extracted "NID" -- producing the "TG:--
                // CH:--" TDU_LC spam in the Live Activity feed
                // even with no real call.
                //
                // We still update `nid_events` above so we can see
                // the raw rate on /api/traffic_lsm diagnostics,
                // but from here we're a no-op.
                use std::sync::atomic::Ordering;
                if traffic_heartbeat_imbe
                    .current_talkgroup
                    .load(Ordering::Relaxed) == 0
                {
                    continue;
                }

                // Dispatch by DUID. Only dispatch on valid NIDs --
                // BCH-failed NIDs aren't trustworthy enough to drive
                // call-state transitions.
                if !status.nid_valid {
                    if total_polls % 64 == 0 || total_polls < 16 {
                        tracing::debug!(
                            target: "p25_traffic_lsm",
                            "NID event with nid_valid=false n_errors={} sync_dist={}",
                            status.n_errors, status.sync_distance,
                        );
                    }
                    continue;
                }

                let now = std::time::Instant::now();
                let locked_tg_snapshot = {
                    let mut mgr = traffic_lsm_mgr.lock().await;
                    match duid {
                        0x0 => mgr.hdu_received(now, nac),
                        0x3 => mgr.tdu_received(now, nac, false),
                        0xF => mgr.tdu_received(now, nac, true),
                        0x5 => mgr.ldu_received(now, nac, false),
                        0xA => mgr.ldu_received(now, nac, true),
                        _ => {
                            mgr.last_duid = Some(duid);
                            mgr.last_nac = Some(nac);
                        }
                    }
                    mgr.current_talkgroup().map(|t| t.0).unwrap_or(0)
                };

                // Log the coarse call boundaries so the event log
                // reads like a call transcript. LDUs are too frequent
                // (1 every ~30 ms) to log individually -- the vocoder
                // task below logs per-call summaries instead.
                //
                // Phase 7F.3 (2026-04-14): only log when we're
                // actually locked on a TG. During Idle the traffic
                // LSM HDL chain keeps firing NID events on residual
                // dibits from the last-tuned frequency; they get
                // classified by the BCH decoder (often as 0xF TDU_LC
                // when the signal is marginal) and would otherwise
                // flood the event log with ~20 TG=0 TDU_LC entries
                // per second. Counters still update in the dispatch
                // match above -- this only gates the Logs-tab spam.
                if locked_tg_snapshot != 0 {
                    match duid {
                        0x0 => traffic_event_log.push(
                            crate::event_log::LogCategory::Imbe,
                            format!("HDU TG={} NAC=0x{:03X}", locked_tg_snapshot, nac),
                            serde_json::json!({
                                "duid":    "HDU",
                                "tg":      locked_tg_snapshot,
                                "nac":     nac,
                            }),
                        ),
                        0x3 | 0xF => traffic_event_log.push(
                            crate::event_log::LogCategory::Imbe,
                            format!(
                                "{} TG={} NAC=0x{:03X}",
                                if duid == 0xF { "TDU_LC" } else { "TDU" },
                                locked_tg_snapshot, nac,
                            ),
                            serde_json::json!({
                                "duid":    if duid == 0xF { "TDU_LC" } else { "TDU" },
                                "tg":      locked_tg_snapshot,
                                "nac":     nac,
                            }),
                        ),
                        _ => {}
                    }
                }

                // Broadcast traffic DUID events to the WebSocket
                // activity feed so the dashboard shows HDU/LDU/TDU.
                //
                // Note: this is unreachable when Idle because the
                // Idle gate above `continue`s the loop before we
                // get here. Kept non-conditional so the dashboard
                // gets every event the heartbeat dispatches,
                // matching the user's "don't suppress activity"
                // requirement -- any event that makes it this far
                // represents a real locked call.
                {
                    let mgr = traffic_lsm_mgr.lock().await;
                    let duid_label = match duid {
                        0x0 => "HDU",
                        0x3 => "TDU",
                        0x5 => "LDU1",
                        0xA => "LDU2",
                        0xF => "TDU_LC",
                        d => { let _ = d; "DUID?" }
                    };
                    let tg = mgr.current_talkgroup()
                        .map(|t| format!("TG:{:05}", t.0))
                        .unwrap_or_else(|| "--".into());
                    let ch = mgr.current_channel()
                        .map(|c| format!("{}", c))
                        .unwrap_or_else(|| "--".into());
                    let now_str = {
                        let d = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default();
                        let total_secs = d.as_secs();
                        let millis = d.subsec_millis();
                        let h = (total_secs / 3600) % 24;
                        let m = (total_secs / 60) % 60;
                        let s = total_secs % 60;
                        format!("{:02}:{:02}:{:02}.{:03}", h, m, s, millis)
                    };
                    let evt = serde_json::json!({
                        "timestamp": now_str,
                        "event_type": format!("TRF_{}", duid_label),
                        "summary": format!("{} NAC:0x{:03X} {} CH:{}",
                            duid_label, nac, tg, ch),
                        "talkgroup": mgr.current_talkgroup().map(|t| t.0),
                        "channel": ch,
                    });
                    if let Ok(json) = serde_json::to_string(&evt) {
                        let _ = traffic_event_tx.send(json);
                    }
                }

                // Periodic log so the on-target dashboard log shows
                // we're seeing NID events.
                if nid_events <= 10 || nid_events % 50 == 0 {
                    let mgr = traffic_lsm_mgr.lock().await;
                    tracing::info!(
                        target: "p25_traffic_lsm",
                        "NID #{nid_events}: NAC=0x{:03X} DUID=0x{:X} \
                         (hdus={} ldus={} tdus={} state={})",
                        nac, duid,
                        mgr.hdus_seen, mgr.ldus_seen, mgr.tdus_seen,
                        mgr.state_label(),
                    );
                }

                // Stash the latest NAC into traffic_stats for the
                // /api/traffic snapshot (the manager has the per-DUID
                // counters; this is just an extra dashboard surface).
                let _ = traffic_lsm_stats.lock().await; // touch to satisfy unused-import
            }
        });

        // 8. Spawn periodic grant-expiry task. The control channel decoder
        //    accumulates voice grants in a HashMap as it sees TSBK_GRANT
        //    messages. Without periodic pruning the table only ever grows
        //    -- the dashboard's "Active Grants" count would never decay
        //    even after a call ended. Expire any grant whose TSBK was last
        //    seen more than 30 seconds ago (P25 typical call timeout).
        let expiry_decoder = decoder.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                let mut dec = expiry_decoder.write().await;
                dec.expire_grants(30);
            }
        });

        // Phase 6F.1: same expiry sweep for the LSM decoder. Without it,
        // grants accumulated by the LSM decoder (which now feeds the
        // dashboard's Active Grants panel) would never time out.
        let lsm_expiry_decoder = lsm_decoder.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                let mut dec = lsm_expiry_decoder.write().await;
                dec.expire_grants(30);
            }
        });

        (ip_core, ad9361)
    };

    // Phase 7E: audio broadcast channel (vocoder -> HTTP/WebSocket).
    let audio_tx = audio::audio_channel();

    // Phase 7D/7E: vocoder task -- reads IMBE frame batches, decodes
    // via mbelib, pushes AudioChunks to the broadcast channel, and
    // updates stats atomics. Encryption gating: encrypted frames are
    // counted but not decoded.
    //
    // Phase 7F.1 (2026-04-14): per-call summary lines written to the
    // structured event log. A "call" is defined by the vocoder reset
    // flag (raised by the follower on Idle->Active transitions);
    // between resets we accumulate frame count + PCM sample count +
    // error count per TG and flush a "call_end" summary on reset.
    {
        let voc_forwarder = imbe_forwarder.clone();
        let voc_audio_tx = audio_tx.clone();
        let voc_event_log = event_log.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            let mut decoder = vocoder::JmbeDecoder::new();
            let mut rx = imbe_rx;
            let mut seq: u64 = 0;

            // Per-call accumulators. `call_tg` is the TG the current
            // accumulator belongs to; flushed on reset or TG change.
            let mut call_tg: u16 = 0;
            let mut call_frames_in: u32 = 0;
            let mut call_frames_skipped_enc: u32 = 0;
            let mut call_pcm_samples: u64 = 0;
            let mut call_started: Option<std::time::Instant> = None;
            // Phase 9.1 (2026-04-15): track the wall clock of the
            // most recent IMBE frame we decoded so `duration_ms` in
            // the call_end summary reflects the actual
            // voice-arrival span, not the full retune-to-retune
            // interval. Before Phase 9.1, duration_ms used
            // `started.elapsed()` which is "time since the first
            // frame of this call was decoded" -- if the follower
            // stayed locked on a TG for 97 s with only 900 ms of
            // real voice and the rest silence+noise-TDU_LCs, the
            // duration was reported as 97244 ms (retune-to-retune
            // wall clock) instead of ~900 ms (actual audio).
            let mut call_last_frame_at: Option<std::time::Instant> = None;

            let flush_call_summary = |
                tg: u16,
                frames_in: u32,
                frames_skipped_enc: u32,
                pcm_samples: u64,
                started: Option<std::time::Instant>,
                last_frame_at: Option<std::time::Instant>,
                log: &std::sync::Arc<crate::event_log::EventLog>,
            | {
                if frames_in == 0 && frames_skipped_enc == 0 {
                    return;
                }
                // Phase 9.1: duration = time from first decoded
                // frame to last decoded frame. When only one burst
                // of voice lives inside a long retune-to-retune
                // lock, this shows the real voice length. Falls
                // back to 0 if we somehow flushed without ever
                // latching a frame timestamp (shouldn't happen when
                // frames_in > 0, but be safe).
                let duration_ms = match (started, last_frame_at) {
                    (Some(s), Some(l)) => {
                        l.duration_since(s).as_millis() as u64
                    }
                    _ => 0,
                };
                log.push(
                    crate::event_log::LogCategory::Vocoder,
                    format!(
                        "call_end TG={} frames={} pcm={} ({} ms){}",
                        tg, frames_in, pcm_samples, duration_ms,
                        if frames_skipped_enc > 0 {
                            format!(" enc_skipped={}", frames_skipped_enc)
                        } else {
                            String::new()
                        },
                    ),
                    serde_json::json!({
                        "tg":                 tg,
                        "frames_in":          frames_in,
                        "frames_skipped_enc": frames_skipped_enc,
                        "pcm_samples":        pcm_samples,
                        "duration_ms":        duration_ms,
                    }),
                );
            };

            tracing::info!(target: "p25_vocoder", "vocoder task started");
            while let Some(frames) = rx.recv().await {
                // Reset on call boundary (raised by the follower on
                // new retune).
                if voc_forwarder.vocoder_reset_pending.swap(false, Ordering::Relaxed) {
                    flush_call_summary(
                        call_tg, call_frames_in, call_frames_skipped_enc,
                        call_pcm_samples, call_started, call_last_frame_at,
                        &voc_event_log,
                    );
                    decoder.reset();
                    call_tg = voc_forwarder
                        .current_talkgroup.load(Ordering::Relaxed);
                    call_frames_in = 0;
                    call_frames_skipped_enc = 0;
                    call_pcm_samples = 0;
                    call_started = Some(std::time::Instant::now());
                    call_last_frame_at = None;
                    voc_event_log.push(
                        crate::event_log::LogCategory::Vocoder,
                        format!("call_start TG={}", call_tg),
                        serde_json::json!({ "tg": call_tg }),
                    );
                }
                let encrypted = voc_forwarder.call_encrypted.load(Ordering::Relaxed);
                if encrypted {
                    voc_forwarder
                        .vocoder_frames_encrypted
                        .fetch_add(9, Ordering::Relaxed);
                    call_frames_skipped_enc += 9;
                    continue;
                }
                let tg = voc_forwarder.current_talkgroup.load(Ordering::Relaxed);
                // Auto-flush if TG changed without an explicit reset
                // (e.g. follower mid-call TG reassignment).
                if tg != call_tg && (call_frames_in > 0 || call_started.is_some()) {
                    flush_call_summary(
                        call_tg, call_frames_in, call_frames_skipped_enc,
                        call_pcm_samples, call_started, call_last_frame_at,
                        &voc_event_log,
                    );
                    call_tg = tg;
                    call_frames_in = 0;
                    call_frames_skipped_enc = 0;
                    call_pcm_samples = 0;
                    call_started = Some(std::time::Instant::now());
                    call_last_frame_at = None;
                }
                for frame in &frames {
                    let pcm = decoder.decode_frame(frame);
                    voc_forwarder
                        .vocoder_pcm_produced
                        .fetch_add(vocoder::SAMPLES_PER_FRAME as u64, Ordering::Relaxed);
                    call_frames_in += 1;
                    call_pcm_samples += vocoder::SAMPLES_PER_FRAME as u64;
                    // Phase 9.1: latch the wall clock of this frame
                    // so the next flush reports the true voice
                    // span instead of the retune-to-retune gap.
                    call_last_frame_at = Some(std::time::Instant::now());
                    // Push to audio broadcast (ignore if no subscribers)
                    let _ = voc_audio_tx.send(audio::AudioChunk {
                        pcm,
                        seq,
                        talkgroup: tg,
                    });
                    seq += 1;
                }
            }
            tracing::warn!(target: "p25_vocoder", "vocoder task exiting (channel closed)");
        });
    }

    // Build app state
    let state = Arc::new(httpd::AppState {
        decoder: decoder.clone(),
        lsm_decoder: lsm_decoder.clone(),
        // Phase 9: `iq_lsm_decoder` + `lsm_stats` removed.
        event_tx,
        #[cfg(target_os = "linux")]
        ip_core,
        #[cfg(target_os = "linux")]
        ad9361,
        // Boot-time front-end config snapshot, used by /api/reinit to
        // restore the chip + DDC NCO without a board reboot.
        boot_rx_lo:        args.rx_lo,
        boot_sample_rate:  args.sample_rate as u32,
        boot_rf_bandwidth: args.rf_bandwidth,
        boot_control_freq: args.control_freq,
        boot_lo_ppm:       args.lo_ppm,
        boot_hardwaregain: args.hardwaregain,
        hdl_lsm: hdl_lsm.clone(),
        irq_stats: irq_stats.clone(),
        // Phase 7A.1: traffic-channel grant follower + dibit reader
        traffic_manager: traffic_manager.clone(),
        traffic_stats: traffic_stats.clone(),
        traffic_follower_enabled: traffic_follower_enabled.clone(),
        // Phase 7C: traffic LSM voice decoder + IMBE counter
        traffic_lsm_decoder: traffic_lsm_decoder.clone(),
        imbe_forwarder: imbe_forwarder.clone(),
        monitor_list: monitor_list.clone(),
        audio_tx: audio_tx.clone(),
        audio_ws_lag_total: std::sync::Arc::new(
            std::sync::atomic::AtomicU64::new(0),
        ),
        boot_instant: std::time::Instant::now(),
        event_log: event_log.clone(),
    });

    // Start HTTP server
    let app = httpd::router(state);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!("Dashboard at http://{}", args.listen);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Safe cast of a byte buffer to u64 slice (assumes alignment from DMA).
#[cfg(target_os = "linux")]
fn bytemuck_cast(buffer: &[u8]) -> &[u64] {
    let len = buffer.len() / 8;
    if len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u64, len) }
}
