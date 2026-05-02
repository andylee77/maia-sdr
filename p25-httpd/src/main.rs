//! Fishball P25 Trunking Radio - PS Application
//!
//! Runs on the Zynq-7020 ARM cores. Responsibilities:
//! - Configure AD9361 via IIO
//! - Configure FPGA DDC + demod via UIO registers
//! - Read dibit DMA stream from FPGA
//! - Decode P25 control channel (sync, TSBK parsing, state machine)
//! - Serve web UI for monitoring talkgroups and grants

// 2026-04-26: bumped from default 128 because the /api/traffic
// response object hit the recursion limit on the serde_json::json!
// macro after adding the `agc_freq_cache` field. 256 gives plenty
// of headroom for further additions.
#![recursion_limit = "256"]

use std::sync::Arc;

use clap::Parser;
use tokio::sync::{broadcast, RwLock};

mod app;
mod audio;
mod hardware;
mod httpd;
mod jmbe;
mod lsm;
mod protocol;
mod services;
mod sw_demod;
mod vocoder;

use app::imbe_forwarder::ImbeForwarder;
use app::vocoder_task;
use audio::recorder;
#[cfg(target_os = "linux")]
use hardware::{fpga, iio};
use protocol::p25;
use protocol::p25::control_channel::ControlChannelDecoder;
use services::{monitor, ntp};

/// Build tag, logged at startup and exposed via `/api/system`.
///
/// Bump this whenever a feature flag changes so on-target verification
/// ("is this the binary I just flashed?") is a trivial grep. Buildroot
/// zeroes mtimes and doc-comment strings don't survive into the binary.
pub const BUILD_TAG: &str = "2026-05-03-same-freq-no-reset";

// ── Runtime / timing constants ─────────────────────────────────────
//
// Values are in milliseconds (suffix `_MS`) or seconds (suffix `_SECS`)
// to match the `Duration::from_millis` / `Duration::from_secs`
// constructor at the call site. Only named here if the literal is
// non-obvious in context or appears in more than one call site; one-off
// obvious durations (e.g. the 5 s NTP timeout, 1 s elapsed() gates
// whose meaning is clear from the surrounding log message) remain
// inline.

/// HDL LSM heartbeat poll tick (~60 Hz). Used by both the control-
/// channel and traffic-chain heartbeat tasks to sample `lsm_status` /
/// `lsm_debug` registers. Matches the ~60 Hz loop called out in the
/// task's banner log.
const LSM_HEARTBEAT_TICK_MS: u64 = 16;

/// NID-event log throttle on the control-channel heartbeat. A busy
/// site emits ~70 NIDs/s; 200 ms caps verbose logging at 5 Hz while
/// still capturing sub-second bursts. First 10 events are always
/// logged.
const NID_EVENT_LOG_THROTTLE_MS: u64 = 200;

/// FPGA register stats poll interval — the periodic log task that
/// dumps `dibit_count`, overflow flags, and buffer cursors. Low
/// enough to see drift, high enough to not spam.
const STATS_POLL_INTERVAL_SECS: u64 = 2;

/// Cumulative + snapshot stats for the HDL LSM chain. Populated by the
/// HDL LSM heartbeat task, read by `/api/hdl_lsm`.
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
///
/// The `dibit` / `traffic` fields are retained for /api/irq_stats JSON
/// shape stability (dashboard tolerates stale 0s); they correspond to
/// the retired PS C4FM `dibit_dma` / `traffic_dma` rings and are never
/// incremented post-Phase-10.8.
#[derive(Debug, Clone, Copy, Default)]
pub struct IrqStats {
    pub total: u64,
    pub dibit: u64,
    pub traffic: u64,
    pub iq: u64,
    pub lsm_dibit: u64,
    /// Traffic-side LSM dibit DMA wakeups.
    pub traffic_lsm_dibit: u64,
    /// Traffic-side post-DDC IQ DMA wakeups (mirror of `iq`).
    pub traffic_iq: u64,
    /// Phase 10.8: control-side pre-differential IQ DMA wakeups.
    pub pre_diff_iq: u64,
    /// Phase 10.8: traffic-side pre-differential IQ DMA wakeups.
    pub traffic_pre_diff_iq: u64,
    /// 2026-05-03: pre-DDC raw 8 MSPS / 8 MHz IQ DMA wakeups (PS-side
    /// software P25 stack).
    pub wideband_iq: u64,
    pub last_at_secs_ago: f64,
    /// Set to None until the first IRQ; updated only by the IRQ task.
    pub started_at: Option<std::time::Instant>,
    pub last_at: Option<std::time::Instant>,
}

/// Data-side counters for the traffic DMA path. Distinct from
/// `IrqStats.traffic` (wakeups) — this tracks bytes/dibits actually
/// consumed by the traffic dibit reader. Exposed via `/api/traffic`.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrafficStats {
    pub started_at: Option<std::time::Instant>,
    pub last_at: Option<std::time::Instant>,
    pub wakeups: u64,
    pub total_buffers: u64,
    pub total_bytes: u64,
    pub total_dibits: u64,
    pub dibit_hist: [u64; 4],
    // IMBE frame extraction counters — updated by the `ImbeForwarder`
    // voice handler. Each LDU1/LDU2 yields 9 IMBE frames (~180 ms audio).
    pub hdu_count: u64,
    pub ldu1_count: u64,
    pub ldu2_count: u64,
    pub tdu_count: u64,
    pub tdu_lc_count: u64,
    /// Total IMBE frames pushed to the vocoder channel. Should equal
    /// `(ldu1_count + ldu2_count) * 9`; divergence signals extraction
    /// failure (wrong dibit count / status-strip math off).
    pub imbe_frames_extracted: u64,
    /// Wall-clock of most recent IMBE batch; feeds "frames/sec" UI.
    pub last_imbe_at: Option<std::time::Instant>,
}


#[derive(Parser)]
#[command(name = "p25-httpd", about = "Fishball P25 Trunking Radio")]
struct Args {
    /// HTTP listen address
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: String,

    /// HTTPS listen address. HTTPS server is only started if both
    /// --ssl-cert and --ssl-key are provided; otherwise this argument
    /// is ignored.
    #[arg(long, default_value = "0.0.0.0:8443")]
    listen_https: std::net::SocketAddr,

    /// Path to PEM-encoded SSL certificate for the HTTPS server.
    /// Unless both --ssl-cert and --ssl-key are present the HTTPS
    /// server is not started. Generated at first boot by the
    /// S50p25-httpd-certificates init script into
    /// /mnt/jffs2/p25-httpd.crt on the Tezuka flashed image.
    #[arg(long)]
    ssl_cert: Option<std::path::PathBuf>,

    /// Path to PEM-encoded SSL private key paired with --ssl-cert.
    /// Default target on Tezuka: /mnt/jffs2/p25-httpd.key.
    #[arg(long)]
    ssl_key: Option<std::path::PathBuf>,

    /// Path to PEM-encoded CA certificate. When provided the CA is
    /// served at /ca.crt so browsers can download + trust it to
    /// eliminate the "self-signed" warning. Default target on Tezuka:
    /// /mnt/jffs2/p25-sdr-ca.crt.
    #[arg(long)]
    ca_cert: Option<std::path::PathBuf>,

    /// AD9361 RX LO frequency in Hz at boot. Can be moved live via
    /// `POST /api/tune` (auto mode) or overridden by `POST /api/preset`.
    #[arg(long, default_value_t = 858_100_000)]
    rx_lo: u64,

    /// DDC preset name at boot. Controls AD9361 sample rate, RF
    /// bandwidth, and per-stage FIR coefficients. Every preset
    /// produces 62.5 kSPS at the DDC output; the choice is between
    /// NCO window width (= sample_rate/2) and FPGA / AD9361 load.
    /// Runtime retune via `POST /api/preset`. Known names are listed
    /// by `GET /api/presets`; see `hardware/ddc_presets.rs`.
    #[arg(long, default_value = "8M")]
    preset: String,

    /// P25 control channel frequency in Hz. No default — must be set
    /// per-site via this arg or `POST /api/tune`.
    #[arg(long)]
    control_freq: Option<u64>,

    /// Pluto LO PPM offset for crystal calibration.
    ///
    /// Shifts the DDC NCO (NOT the AD9361 LO request) — the LO
    /// synthesizer step is much coarser than a PPM-scale shift, so a
    /// small LO shift rounds back to nominal while the NCO math still
    /// moves, doubling the post-DDC offset and breaking lock. The DDC
    /// NCO runs at 1 Hz precision and is the only place a sub-step
    /// shift actually lands.
    ///
    /// Negative ppm = slow crystal (signals appear above expected IF).
    /// Match whatever value SDRTrunk's tuner panel reports for the
    /// same radio.
    ///
    /// Math: nco_shift = -ppm * 1e-6 * rx_lo Hz.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    lo_ppm: f64,

    /// AD9361 RX hardware gain in dB. Sets gain_control_mode=manual
    /// and writes this value to `hardwaregain`.
    ///
    /// The Maia HDL LSM chain has no software AGC (SDRTrunk's
    /// `P25P1DemodulatorLSM.java:157-172` does a per-symbol IIR to
    /// OBJECTIVE_MAGNITUDE; we don't). The slicer has fixed integer
    /// thresholds, so front-end gain has to land in a narrow ±5-10 dB
    /// window or outer 4FSK symbols clip (too high) or crowd into
    /// inner bins (too low). AD9361 AGC does NOT converge to this
    /// window on a strong antenna — slow_attack picks 71-73 dB and
    /// fast_attack picks ~0 dB, both yielding very poor CRC pass
    /// rates; manual gain in the 55–60 dB range typically lands in
    /// the lock window on strong signals. Tune per antenna + site.
    ///
    /// Retune via this arg or `POST /api/preset`. A proper HDL
    /// software AGC would eliminate per-antenna tuning. See
    /// doc/changes/040_api_reinit_and_manual_gain.md.
    #[arg(long, default_value_t = 60.0)]
    hardwaregain: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 2026-04-27: default log level was `info,p25_httpd=info`,
    // which produced ~30 lines/sec on a busy CC site (per-NID,
    // per-SYNC-HIT, per-IRQ tracing) and filled the 500 MB tmpfs
    // at /tmp (where /var/log is symlinked) in ~24 hours, killing
    // the daemon when ENOSPC came back from the next write().
    //
    // The file log is unused — operators read /api/log instead,
    // which is a separate in-process ring buffer (services::
    // event_log) that's not subject to tracing filters. So drop
    // the stdout filter to `warn` by default: only real issues
    // hit the file. RUST_LOG env still wins for debugging.
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .init();

    let args = Args::parse();

    // Require --control_freq explicitly. No built-in default — the
    // right value is site-specific and a wrong default would
    // silently decode noise.
    let control_freq: u64 = args.control_freq.ok_or_else(|| {
        anyhow::anyhow!(
            "--control_freq <Hz> is required (P25 control channel frequency \
             for the target site; e.g. --control_freq 851012500)"
        )
    })?;

    // Resolve --preset to a static preset handle. Bails at startup
    // rather than silently falling back to the default so a typo is
    // surfaced immediately.
    let boot_preset = hardware::ddc_presets::find_preset(&args.preset)
        .ok_or_else(|| {
            let names: Vec<&str> = hardware::ddc_presets::PRESETS
                .iter().map(|p| p.name).collect();
            anyhow::anyhow!(
                "unknown --preset '{}'; known presets: {}",
                args.preset,
                names.join(", "),
            )
        })?;
    let boot_preset_idx = hardware::ddc_presets::PRESETS
        .iter()
        .position(|p| p.name == boot_preset.name)
        .expect("resolved preset must be in PRESETS table");

    tracing::info!(
        "p25-httpd build: {} (dashboard_source=lsm_decoder)",
        BUILD_TAG
    );

    // NTP sync early so event_log + grant timestamps + /api/stats
    // wall-clock read as real time rather than the 1970 epoch.
    // Fishball has no battery-backed RTC. Non-blocking: the board
    // must work offline, and event_log ordering is already correct
    // via the monotonic `seq` field when wall_clock_ms is garbage.
    // Cost bound: servers.len() * per_server_timeout = 15 s.
    // Blocking pool so we don't park the tokio runtime.
    match tokio::task::spawn_blocking(|| {
        ntp::sync_system_clock(
            &["pool.ntp.org", "time.cloudflare.com", "time.google.com"],
            std::time::Duration::from_secs(5),
        )
    })
    .await
    {
        Ok(Ok(epoch)) => tracing::info!(
            "NTP sync OK — system clock set to Unix epoch {epoch}"
        ),
        Ok(Err(e)) => tracing::warn!(
            "NTP sync failed ({e}); timestamps will use kernel boot clock"
        ),
        Err(e) => tracing::warn!("NTP task panicked: {e}"),
    }

    // Pluto crystal calibration — shift DDC NCO by
    // -ppm * 1e-6 * rx_lo Hz. Boot order of precedence:
    //   1. --lo-ppm CLI flag if explicitly non-zero (operator override).
    //   2. Persisted /var/lib/p25-httpd/ppm_cal.json from last auto-PPM.
    //   3. CLI default (0.0) -> no shift, relies on first post-boot
    //      auto-PPM to converge.
    // This lets the init script ship `--lo-ppm 0` and still get a
    // correct shift applied across reboots, without a hardcoded
    // per-unit value.
    let mut nco_lo_shift_hz: f64 =
        -args.lo_ppm * 1e-6 * args.rx_lo as f64;
    let mut ppm_source = "cli";
    #[cfg(target_os = "linux")]
    if args.lo_ppm == 0.0 {
        if let Some(p) = app::autoppm::load_persisted() {
            // Sanity-check the persisted shift. AD9361 crystal trim
            // is typically ±1 ppm, occasionally up to ±5. Anything
            // beyond ~800 Hz at an 858 MHz LO (≈1 ppm) is almost
            // certainly garbage from an earlier broken calibration
            // (hit 2026-04-23: a stale file had +1.37 ppm / -1178 Hz
            // after the wideband DT carveout broke stage A). Reject
            // it and fall through to 0 shift rather than permanently
            // lock the PLL out of capture range.
            const MAX_PLAUSIBLE_HZ: f64 = 1000.0;
            let shift = p.lo_shift_hz as f64;
            if shift.abs() <= MAX_PLAUSIBLE_HZ {
                nco_lo_shift_hz = shift;
                ppm_source = "persisted";
                tracing::info!(
                    "auto-PPM: loaded persisted calibration \
                     lo_ppm={:+.4} shift={:+.0} Hz (from {} @ unix {})",
                    p.lo_ppm, p.lo_shift_hz,
                    app::autoppm::PPM_CAL_FILE, p.unix_secs);
            } else {
                tracing::warn!(
                    "auto-PPM: REJECTED persisted calibration \
                     lo_shift_hz={:+.0} (>{:.0} Hz, implausible at \
                     rx_lo={} Hz); falling through to 0 shift",
                    shift, MAX_PLAUSIBLE_HZ, args.rx_lo);
                ppm_source = "persisted-rejected";
            }
        }
    }

    tracing::info!(
        "Fishball P25 starting: RX LO={} Hz, control_freq={} Hz, \
         lo_ppm={:+} ({:+.1} Hz NCO shift, src={})",
        args.rx_lo,
        control_freq,
        args.lo_ppm,
        nco_lo_shift_hz,
        ppm_source
    );

    // 2026-04-30 traffic-PPM-fix: hoist the live DDC NCO crystal-trim
    // shift atomic so the grant follower (spawned below) can read it
    // on every retune. Previously the follower received the static
    // `args.lo_ppm` at spawn and never picked up auto-PPM apply nor
    // boot-loaded persisted shifts — control NCO was corrected,
    // traffic NCO wasn't, leaving the traffic Costas loop with the
    // full residual.
    let current_lo_shift_hz = std::sync::Arc::new(
        std::sync::atomic::AtomicI64::new(
            nco_lo_shift_hz.round() as i64));

    let (event_tx, _) = broadcast::channel::<String>(256);
    let mut decoder = ControlChannelDecoder::new();
    decoder.set_event_tx(event_tx.clone());
    let decoder = Arc::new(RwLock::new(decoder));

    // A second independent `ControlChannelDecoder` fed by the HDL LSM
    // dibit stream. Same pipeline as the C4FM decoder, different dibit
    // source. Shares `event_tx` so both decoders' TSBKs land on the
    // same WS; distinct trace targets separate them in the server log.
    //
    // NOTE: `grant_event_rx` LOOKS unused on Windows (cargo fix will
    // offer to rename it `_grant_event_rx` AND remove `mut`) but it
    // IS consumed via `.recv()` inside a `cfg(target_os = "linux")`
    // block. Do NOT accept either rewrite — they break the Linux build.
    #[allow(unused_variables, unused_mut)]
    let (grant_event_tx, mut grant_event_rx) =
        tokio::sync::mpsc::channel::<p25::events::P25Event>(128);
    let monitor_list = Arc::new(RwLock::new(monitor::MonitorList::default()));

    let mut lsm_decoder = ControlChannelDecoder::new();
    lsm_decoder.set_event_tx(event_tx.clone());
    lsm_decoder.set_grant_event_tx(grant_event_tx.clone());
    let lsm_decoder = Arc::new(RwLock::new(lsm_decoder));

    // Also install the grant event sender on the C4FM decoder so
    // C4FM-site grants reach the follower. On LSM sites the C4FM
    // decoder rejects garbage dibits via BCH+CRC (harmless); on C4FM
    // sites this is the only path that produces grant events.
    // Modulation-mismatch filtering happens in the follower task.
    {
        let mut d = decoder.write().await;
        d.set_grant_event_tx(grant_event_tx);
    }

    // The pure-software Phase 6D LSM pipeline was retired after the
    // HDL LSM chain went green; see doc/changes/039. The `iq_dma`
    // ring stays dormant in the bitstream for future raw-IQ use.

    // Traffic-channel `ControlChannelDecoder` fed by the
    // `traffic_lsm_dibit_dma` ring. Runs on the FOLLOWED VOICE channel
    // and produces HDU/LDU1/LDU2/TDU/TDU_LC events. Voice handler
    // installed below forwards extracted IMBE frames to the vocoder.
    //
    // mpsc channel: 32 LDU batches (~5.8 s audio) absorbs jitter.
    // Sized up from 16 on 2026-04-24 after field observation that a
    // ~4% IMBE drop rate lines up exactly with 17 LDU-batch overflow
    // events over a session where 409 LDUs were extracted. Each
    // JMBE-decode batch takes ~90 ms worst case on this ARM; during
    // a momentary scheduler stall or a run of slow frames the producer
    // bursts past the 16-slot depth. Doubling is cheap — worst-case
    // added latency is only realised if the vocoder actually stalls,
    // and recovery is shorter than the old-build drop.
    //
    // Each batch is a tuple of (talkgroup_at_submission, frames). The
    // TG is captured at send time by the forwarder (not read by the
    // vocoder at receive time) so that tail frames of call N retain
    // their OLD TG even after the follower has retuned and advanced
    // `current_talkgroup` to call N+1. Fixes "end of call audio at
    // start of next call was not saved under actual call" (2026-04-24
    // field observation).
    let (imbe_tx, imbe_rx) =
        tokio::sync::mpsc::channel::<(u16, u32, u64, u64, [p25::voice_frame::ImbeFrameRaw; 9])>(32);
    let imbe_forwarder = Arc::new(ImbeForwarder::new(imbe_tx));

    // Call-boundary broadcast (traffic-LSM heartbeat -> recorder;
    // ImbeForwarder::on_tdu_lc -> recorder). Declared here because
    // the traffic heartbeat task (spawned later) clones this tx.
    let call_boundary_tx = audio::call_boundary_channel();
    // 2026-04-30 sync diagnostic ring. Owned by AppState; cloned into
    // the traffic LSM heartbeat task so per-NID PLL/AGC/sync samples
    // are pushed during active calls. Filtered by call_id at
    // /api/recordings/{id}/sync_trace.
    let sync_trace_ring = crate::services::sync_trace::new_ring();
    // Phase 2c (2026-04-25): the CallTracker broadcast is constructed
    // here so the traffic grant follower (spawned in the cfg(linux)
    // block) can subscribe and release the chain on CallClose. The
    // CallTracker authority task itself is spawned further down where
    // its dependencies (audio_tx + active_call_snapshot) are ready.
    let call_tracker_tx = crate::app::grant_follower::new_event_tx();
    // Lets on_tdu_lc publish Motorola TALK_COMPLETE source stamps.
    imbe_forwarder.set_boundary_tx(call_boundary_tx.clone());
    // `event_log` wiring is deferred until that ring is constructed
    // further down (search "set_event_log(event_log").
    imbe_forwarder.set_ws_event_tx(event_tx.clone());

    let mut traffic_lsm_decoder = ControlChannelDecoder::new();
    traffic_lsm_decoder.set_event_tx(event_tx.clone());
    // IMBE forwarder as voice handler — counts events AND pushes
    // frame batches to the vocoder task via try_send.
    traffic_lsm_decoder.set_voice_handler(imbe_forwarder.clone());
    // 2026-04-30: REVERTED the strict bake-in (sync ≤ 4, BCH ≤ 4).
    // Field-tested on Clay County and broke every call: 5/6 grants
    // produced 0 IMBEs, the 1 that extracted 117 IMBEs synthesised
    // pure silence (peak=10, rms=3.2, 0 % of samples above noise
    // floor). After PUT /api/{bch_t,sync_tune} reset to defaults,
    // calls produced real audio (peak=16469, rms=1103, 78 % above
    // floor). The HDL's t=4 works on its own internal pristine
    // dibits; the PS framer running over dibit DMA needs the wider
    // sphere to recall real LDU NIDs. Defenders: NAC mismatch guard
    // (added in this build) catches the over-correction false
    // positives without throwing away the recall.
    let traffic_lsm_decoder = Arc::new(RwLock::new(traffic_lsm_decoder));

    // Shared structured event log. 16384 entries; with verbose=off
    // (default) only operator-actionable categories land (Vocoder,
    // Traffic, Recorder, Grant, System), so the ring tails several
    // hours of meaningful events. Verbose=on captures every Voice
    // TDULC / per-frame Duid for offline analysis but rolls in
    // seconds. Toggle via `P25_LOG_VERBOSE=1` env var. Either way,
    // `tracing::info!` writes everything to journalctl.
    let event_log = Arc::new(crate::services::event_log::EventLog::new(16384));
    if std::env::var("P25_LOG_VERBOSE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        event_log.set_verbose(true);
        tracing::info!(
            target: "p25_event_log",
            "verbose log mode ON (P25_LOG_VERBOSE) — Voice/Duid events land in ring",
        );
    }
    event_log.push(
        crate::services::event_log::LogCategory::System,
        "p25-httpd startup",
        serde_json::json!({
            "build_tag": crate::BUILD_TAG,
        }),
    );
    // Now the event-log ring exists — plumb it into the IMBE
    // forwarder so TDULC LCW parses (Motorola `TALK_COMPLETE` +
    // Standard GVCU) emit Activity-feed entries alongside
    // HDU/LDU/TDU heartbeat lines.
    imbe_forwarder.set_event_log(event_log.clone());

    // Pipe event_log + chain label into each ControlChannelDecoder so
    // every successful NID decode emits one `Duid` category entry.
    // `/api/log?category=duid` then returns a timestamped decode trail
    // across both chains. Chain labels match the dashboard's
    // "decoder_compare" table.
    {
        let mut d = decoder.write().await;
        d.event_log = Some(event_log.clone());
        d.chain_label = "ps_c4fm";
    }
    {
        let mut d = lsm_decoder.write().await;
        d.event_log = Some(event_log.clone());
        d.chain_label = "control";
    }
    {
        let mut d = traffic_lsm_decoder.write().await;
        d.event_log = Some(event_log.clone());
        d.chain_label = "traffic";
    }

    // Shared PL HDL LSM runtime + IRQ stats. Populated by their
    // respective tasks, read by /api/hdl_lsm and /api/irq_stats.
    // Declared out of cfg(linux) so AppState builds on every target.
    let hdl_lsm = Arc::new(tokio::sync::Mutex::new(HdlLsmRuntime::default()));
    let irq_stats = Arc::new(tokio::sync::Mutex::new(IrqStats::default()));

    // Traffic-channel grant follower + dibit-reader stats. Created out
    // of cfg(linux) so AppState sees them on every target. The tasks
    // that touch ip_core live INSIDE cfg(linux).
    let traffic_chain = Arc::new(tokio::sync::Mutex::new(
        p25::traffic_chain::TrafficChain::new(
            args.rx_lo, boot_preset.sample_rate_hz as u64),
    ));
    let traffic_stats = Arc::new(tokio::sync::Mutex::new(TrafficStats::default()));
    // When `false`, the grant follower skips its entire loop iteration
    // (no retune, no timeout sweep). User flips via
    // `GET /api/traffic?follower=off` to take manual control of the
    // traffic DDC. Process-lifetime only, does not persist.
    let traffic_follower_enabled =
        Arc::new(std::sync::atomic::AtomicBool::new(true));
    // 2026-04-24 diagnostic: when true, follower will not dispatch
    // retunes — chain stays on whatever freq is set. Off by default.
    let traffic_lock_freq =
        Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Live RX LO, sample rate, preset index, center-lock. Initialised
    // from CLI, updated by the /api/preset and /api/tune handlers.
    // The grant follower reads current_rx_lo and current_sample_rate_hz
    // on every retune so its NCO-offset math stays correct after a
    // preset change or a scanner-mode LO recenter.
    let current_rx_lo = Arc::new(std::sync::atomic::AtomicI64::new(
        args.rx_lo as i64,
    ));
    let current_sample_rate_hz = Arc::new(
        std::sync::atomic::AtomicU32::new(boot_preset.sample_rate_hz),
    );
    let current_preset_idx = Arc::new(
        std::sync::atomic::AtomicUsize::new(boot_preset_idx),
    );
    let center_locked = Arc::new(
        std::sync::atomic::AtomicBool::new(false),
    );

    // P25 modulation: 0 = Auto (probing), 1 = C4FM, 2 = LSM. Defaults
    // to LSM (simulcast sites are the common case). Declared here so
    // the grant follower can clone it; auto-detect task (writer)
    // spawned later.
    let active_modulation =
        Arc::new(std::sync::atomic::AtomicU8::new(2));

    #[cfg(target_os = "linux")]
    let (ip_core, ad9361, wideband_iq_capture, sw_demod_enabled, sw_demod_stats) = {
        use tokio::sync::Mutex;

        // 1. Initialize FPGA IP core via UIO
        let (ip_core, interrupt_handler) = fpga::IpCore::take().await?;
        tracing::info!("FPGA IP core initialized");

        // Configure AD9361 via IIO. Sample rate and RF bandwidth come
        // from the boot preset (ddc_presets.rs). Manual gain is
        // deliberate — see the doc comment on Args::hardwaregain.
        // doc/changes/040 has the live measurement sweep.
        let ad9361 = iio::Ad9361::new().await?;
        ad9361.set_rx_lo_frequency(args.rx_lo).await?;
        ad9361
            .set_sampling_frequency(boot_preset.sample_rate_hz)
            .await?;
        ad9361
            .set_rx_rf_bandwidth(boot_preset.rf_bandwidth_hz)
            .await?;
        ad9361
            .set_rx_gain_mode(iio::GainMode::Manual)
            .await?;
        ad9361.set_rx_gain(args.hardwaregain).await?;
        tracing::info!(
            "AD9361 configured: preset={} LO={} Hz, Fs={} Hz, BW={} Hz, \
             gain_mode=manual, hardwaregain={} dB",
            boot_preset.name,
            args.rx_lo,
            boot_preset.sample_rate_hz,
            boot_preset.rf_bandwidth_hz,
            args.hardwaregain,
        );

        // Configure control DDC (FIR + decimation + NCO). lo_ppm
        // crystal calibration folds into the NCO — see Args::lo_ppm.
        let nco_offset =
            control_freq as f64 - args.rx_lo as f64 + nco_lo_shift_hz;
        ip_core.configure_ddc(nco_offset, boot_preset)?;
        ip_core.set_ddc_enable(true);
        // iq_dma ring feeds /api/spectrum (software FFT over post-DDC
        // IQ on ARM). 250 KB/s DDR + ~8 IRQ/s.
        ip_core.set_iq_dma_enable(true);
        // HDL LSM demod chain runs alongside the C4FM demod on the
        // same control DDC output. Front-end DC blocker is
        // production-correct (see doc/changes/031); without it the
        // slicer sees a 60/40 inner/outer dibit ratio for ~2-3 min
        // after PLL start.
        ip_core.set_lsm_enable(true);
        ip_core.set_lsm_dibit_dma_enable(true);
        ip_core.set_lsm_dc_block_enable(true);
        // Per-symbol LSM AGC — fixed-point port of SDRTrunk's
        // P25P1DemodulatorLSM.java AGC (L2 sqrt magnitude,
        // `req_gain = 1.0 / mag`, 0.05 IIR lerp, asymmetric clamp
        // at 500). See maia-hdl/p25_hdl/lsm_agc.py + doc/changes/040.
        ip_core.set_lsm_agc_enable(true);
        // Read back lsm_control to confirm the bits actually stuck in the
        // register bank. If the readback disagrees with what we wrote we
        // have a register-bank wiring bug (rare; would surface as obvious
        // garbage in lsm_status / lsm_nid downstream).
        let (lsm_en_rb, lsm_dma_en_rb, lsm_dc_block_rb, lsm_agc_rb) =
            ip_core.lsm_control_readback();
        tracing::info!(
            "Control DDC: offset={nco_offset} Hz, dibit + iq + lsm ring DMA enabled \
             (lsm_control readback: lsm_enable={lsm_en_rb}, \
             lsm_dibit_dma_enable={lsm_dma_en_rb}, \
             lsm_dc_block_enable={lsm_dc_block_rb}, \
             lsm_agc_enable={lsm_agc_rb})"
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

        // 2026-05-03 dual-DDC pivot: traffic side rebuilt around a
        // dedicated `traffic_ddc` (mirror of the control DDC). FIRs +
        // decim + initial NCO=0 are loaded at boot; per-call retunes
        // just write the NCO via `retune_traffic_chain`.
        ip_core.set_pre_diff_iq_dma_enable(true);
        // Configure traffic DDC with the same boot preset as the
        // control side. NCO starts at 0 (no target); the first grant
        // from the follower writes it to `grant_freq - traffic_lo`.
        ip_core.configure_traffic_ddc(0.0, boot_preset)?;
        ip_core.set_traffic_ddc_enable(true);
        // Traffic post-DDC IQ ring (50 kSPS, mirror of `iq_dma`)
        // feeds /api/spectrum?chain=traffic.
        ip_core.set_traffic_iq_dma_enable(true);
        // Traffic pre-diff IQ ring (mirror of `pre_diff_iq_dma`)
        // feeds Plots tab traffic-side eye / constellation.
        ip_core.set_traffic_pre_diff_iq_dma_enable(true);
        // Arm the traffic LSM chain without enabling its master
        // gate. The grant follower flips traffic_lsm_enable on per
        // call via `retune_traffic_chain` and off in `pause_traffic_chain`.
        ip_core.set_traffic_lsm_enable(false);
        ip_core.set_traffic_lsm_dibit_dma_enable(true);
        ip_core.set_traffic_lsm_dc_block_enable(true);
        ip_core.set_traffic_lsm_agc_enable(true);
        let (tlsm_en_rb, tlsm_dma_en_rb, tlsm_dc_block_rb, tlsm_agc_rb) =
            ip_core.traffic_lsm_control_readback();
        tracing::info!(
            "Traffic chain armed (dual-DDC): traffic_lsm_enable={tlsm_en_rb} \
             (off until first retune), \
             traffic_lsm_dibit_dma_enable={tlsm_dma_en_rb}, \
             traffic_lsm_dc_block_enable={tlsm_dc_block_rb}, \
             traffic_lsm_agc_enable={tlsm_agc_rb}"
        );
        if tlsm_en_rb || !tlsm_dma_en_rb {
            tracing::error!(
                "traffic_lsm_control readback mismatch -- expected \
                 enable=false and dibit_dma_enable=true, got \
                 ({tlsm_en_rb},{tlsm_dma_en_rb})"
            );
        }
        // Wideband spectrometer runs pre-DDC on `rxiq_cdc`,
        // independent of every demod. Default 256 integrations at
        // 8 MSPS = ~8 Hz update cadence.
        ip_core.set_wideband_spec_integrations(256);
        ip_core.set_wideband_spec_peak_detect(false);
        ip_core.set_wideband_spec_enable(true);
        let _ = boot_preset;

        let ip_core = Arc::new(Mutex::new(ip_core));
        let ad9361 = Arc::new(ad9361);

        // Interrupt waiters before spawning handler. The PS C4FM
        // dibit / traffic_dma rings were retired in Phase 10.8 along
        // with the HDL C4FM chain, so only the LSM dibit waiters +
        // the post-DDC IQ waiter (used implicitly via /ws/iq polling)
        // are live. The `decoder` ControlChannelDecoder stays alive
        // as an API-level placeholder (it just never gets fed dibits
        // now) so the dashboard's modulation-picker UI still resolves.
        let lsm_dibit_waiter = interrupt_handler.waiter_lsm_dibit_dma();
        let traffic_lsm_dibit_waiter =
            interrupt_handler.waiter_traffic_lsm_dibit_dma();
        let wideband_iq_waiter =
            interrupt_handler.waiter_wideband_iq_dma();
        let _ = &decoder; // keep the binding live for the Auto-mod probe task

        // Interrupt handler.
        let irq_stats_for_handler = irq_stats.clone();
        tokio::spawn(async move {
            if let Err(e) = interrupt_handler.run(irq_stats_for_handler).await {
                tracing::error!("interrupt handler error: {e}");
            }
        });

        // HDL LSM dibit drain + TSBK decode — the single production
        // source of truth for control-channel TSBK parsing. Formerly
        // paralleled by a PS C4FM reader fed from the retired
        // `dibit_dma` ring; that path was removed in Phase 10.8.
        app::dibit_readers::spawn_hdl_lsm_control_reader(
            lsm_dibit_waiter,
            ip_core.clone(),
            lsm_decoder.clone(),
        );

        // M2B 2026-05-02: spawn_hdl_lsm_traffic_reader restored,
        // fed off the new mux-based dibit DMA ring.
        app::dibit_readers::spawn_hdl_lsm_traffic_reader(
            traffic_lsm_dibit_waiter,
            ip_core.clone(),
            traffic_lsm_decoder.clone(),
            imbe_forwarder.clone(),
        );

        // 2026-05-03: wideband raw IQ reader (PS-side software P25
        // stack stage 1). Drains the new 8 MSPS / 8 MHz BW IQ DMA
        // ring fed straight from rxiq_cdc. Logs throughput, optionally
        // captures to tmpfs for offline analysis, AND tees Complex32
        // chunks to the live software demod task (Stage 2B).
        let wideband_iq_capture =
            std::sync::Arc::new(app::wideband_iq_task::WidebandIqCaptureState::default());

        // mpsc to the live software demod. Bounded — drop on full
        // (the wideband_iq_task tracks dropped count). 64 chunks @
        // ~30 chunks/s = ~2 s of slack before dropping.
        let (sw_demod_tx, sw_demod_rx) =
            tokio::sync::mpsc::channel::<Vec<crate::lsm::Complex32>>(64);
        // 2026-05-03 dual-DDC pivot: sw_demod is no longer the
        // production path — the HDL traffic chain (dedicated
        // `traffic_ddc` mirror of the control DDC) drives audio.
        // Default to OFF so the wideband_iq DMA + MultistageDdc +
        // LsmPipeline don't burn ~50-70% CPU servicing chunks the
        // framer never consumes. Operator can re-enable for offline
        // capture / SDRTrunk-bit-exact diagnostics via
        // `POST /api/sw_demod?enabled=1`.
        let sw_demod_enabled =
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sw_demod_stats =
            std::sync::Arc::new(app::sw_demod_task::SwDemodStats::default());

        app::wideband_iq_task::spawn_wideband_iq_reader(
            wideband_iq_waiter,
            ip_core.clone(),
            wideband_iq_capture.clone(),
            Some(sw_demod_tx),
        );

        app::sw_demod_task::spawn_sw_demod(
            sw_demod_rx,
            sw_demod_enabled.clone(),
            ip_core.clone(),
            traffic_chain.clone(),
            traffic_lsm_decoder.clone(),
            current_rx_lo.clone(),
            current_lo_shift_hz.clone(),
            sw_demod_stats.clone(),
        );

        // HDL LSM heartbeat + NID event poller. Reads lsm_status +
        // lsm_debug on every 16ms tick (~60 Hz) — lets us see the
        // chain's state even when it isn't producing NIDs. Emits:
        //   1. NID event log (throttled 5 Hz on a busy site)
        //   2. Heartbeat log (every ~1 s, windowed pll/sp/sync_dist
        //      + iq_dma health)
        //   3. Crash dump — first time nid_evts==0 after healthy
        //      traffic, dumps the full 32-deep NID ring buffer.
        //
        // No watchdog: sdr_reset is unsafe during operation — resets
        // the sync clock domain mid-DMA and deadlocks the AXI HP
        // slave, causing kernel panic reboot. See doc/changes/024.
        // Recovery from stall requires a power cycle.
        let lsm_nid_core = ip_core.clone();
        let lsm_nid_runtime = hdl_lsm.clone();
        tokio::spawn(async move {
            tracing::info!("HDL LSM heartbeat + NID poller task started");
            // Stamp the start time as soon as we run.
            {
                let mut rt = lsm_nid_runtime.lock().await;
                rt.started_at = Some(std::time::Instant::now());
            }
            let mut tick = tokio::time::interval(
                std::time::Duration::from_millis(LSM_HEARTBEAT_TICK_MS),
            );
            tick.tick().await;

            // NID event tracking (cumulative).
            let mut event_count: u64 = 0;
            let mut valid_count: u64 = 0;
            let mut last_drop_count: u16 = 0;
            let mut last_event_log = std::time::Instant::now();

            // NID event ring buffer for crash-transition dump —
            // last 32 events with full state.
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

            // iq_dma drain-rate tracking for the heartbeat.
            let mut last_iq_next_addr: u32 = 0;
            let mut last_iq_last_buffer: u8 = 0xFF;
            let mut hb_iq_addr_advance: u64 = 0;
            let mut hb_iq_buffer_changes: u32 = 0;
            let mut hb_iq_overflow_ticks: u32 = 0;
            let mut last_iq_overflow_log = std::time::Instant::now();

            // Heartbeat windowed stats — reset each emission
            // (~1 s = ~60 ticks at 16 ms).
            let mut hb_ticks: u32 = 0;
            let mut hb_pll_min: i16 = i16::MAX;
            let mut hb_pll_max: i16 = i16::MIN;
            let mut hb_sp_min: i16 = i16::MAX;
            let mut hb_sp_max: i16 = i16::MIN;
            // sync_distance is u8 0..47; track BEST (lowest) in window.
            // 99 = "no observations yet" sentinel.
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

                // Snapshot status + debug + iq_dma health coherently
                // under the mutex so the heartbeat observes the same
                // instant the nid_event payload would describe.
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

                // iq_dma health: AW address advance + last_buffer
                // rollover rate. Healthy = ~5 KB/tick
                // (62.5kSPS*4B / 60Hz). Stuck or <50% = write side
                // stalled. Ring wraps every 32 KB so a single tick
                // shouldn't advance by more than 8 KB; the 0x10000
                // guard filters spurious backward jumps from CDC races.
                if last_iq_next_addr != 0 {
                    let delta = iq_next_addr.wrapping_sub(last_iq_next_addr);
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
                    // Warning throttled to 1 Hz — a stuck overflow
                    // would otherwise drown the log.
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

                // Fold this tick into the heartbeat window.
                hb_ticks += 1;
                if pll_dbg < hb_pll_min { hb_pll_min = pll_dbg; }
                if pll_dbg > hb_pll_max { hb_pll_max = pll_dbg; }
                if sp_dbg  < hb_sp_min  { hb_sp_min  = sp_dbg; }
                if sp_dbg  > hb_sp_max  { hb_sp_max  = sp_dbg; }
                if status.bch_busy      { hb_bch_busy_ticks += 1; }
                if status.in_nid_window { hb_in_window_ticks += 1; }
                if status.nid_event     { hb_nid_event_ticks += 1; }
                if status.dibit_overflow { hb_overflow_ticks += 1; }
                // Lowest sync_distance across the window, regardless
                // of nid_event. HDL latches at sync-hit so it stays
                // constant between events; rsticky bits decay on read,
                // so min over raw reads is the right aggregate.
                if status.sync_distance < hb_sync_dist_best {
                    hb_sync_dist_best = status.sync_distance;
                }

                // Per-tick: dibit overflow latch. Throttled via the
                // shared iq-side 1 Hz gate (both signal the same
                // upstream stall; we want one warn/second total).
                if status.dibit_overflow {
                    if last_iq_overflow_log.elapsed()
                        >= std::time::Duration::from_secs(1)
                    {
                        // throttle managed iq-side; count only
                    }
                }

                // Live PL register snapshot to shared HdlLsmRuntime
                // so /api/hdl_lsm can read it.
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

                // Per-tick: NID event handling.
                if status.nid_event {
                    event_count += 1;
                    hb_window_event_count += 1;
                    if status.nid_valid {
                        valid_count += 1;
                        hb_window_valid_count += 1;
                    }

                    // Always push into the ring regardless of log
                    // throttling — this is what the crash dump reads.
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

                    // NID-event update to shared runtime.
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
                        || last_event_log.elapsed()
                            >= std::time::Duration::from_millis(NID_EVENT_LOG_THROTTLE_MS);
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

                // Heartbeat log (every ~1 s of polling).
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

                    // Crash-transition NID ring dump. First heartbeat
                    // with zero NIDs after any healthy one — dump the
                    // full 32-deep ring with pll/sp/sync_dist/n_errors/
                    // drop_count, no throttling. Fires once per boot.
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

                    // Snapshot completed window into shared runtime
                    // BEFORE we reset accumulators.
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

        // Periodic stats task — polls FPGA registers every 2s.
        let stats_core = ip_core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(
                std::time::Duration::from_secs(STATS_POLL_INTERVAL_SECS));
            tick.tick().await; // discard first immediate tick
            loop {
                tick.tick().await;
                let core = stats_core.lock().await;
                tracing::info!(
                    target: "p25_stats",
                    "regs: lsm_last_buffer={} lsm_next_addr=0x{:08X}",
                    core.lsm_dibit_last_buffer(),
                    core.lsm_dibit_next_address(),
                );
            }
        });

        // `spawn_ps_c4fm_traffic_reader` was retired in Phase 10.8 —
        // the `traffic_dma` ring and `traffic_demod_*` registers are
        // gone. `traffic_stats` (dibit histogram, byte counters) stays
        // in AppState so the dashboard's /api/traffic panel keeps its
        // JSON shape; it just never gets populated now.
        let _ = &traffic_stats;

        // Traffic grant follower — body in `crate::app::follower`.
        // See that module for polling / sticky-lock / encryption
        // policy rationale. Phase 2c (2026-04-25) wires the
        // CallTrackerEvent broadcast into the follower so it can
        // release the chain on CallClose.
        app::grant_follower::spawn_grant_follower(
            traffic_chain.clone(),
            ip_core.clone(),
            current_sample_rate_hz.clone(),
            current_rx_lo.clone(),
            current_lo_shift_hz.clone(),
            traffic_follower_enabled.clone(),
            imbe_forwarder.clone(),
            monitor_list.clone(),
            event_log.clone(),
            traffic_lsm_decoder.clone(),
            grant_event_rx,
            traffic_lock_freq.clone(),
            call_boundary_tx.clone(),
            call_tracker_tx.clone(),
        );

        // M2B 2026-05-02: traffic LSM heartbeat task restored. Polls
        // traffic_lsm_status @ 16 ms — fine enough we don't miss
        // back-to-back NIDs (~14 ms apart). On nid_event, dispatches
        // NAC+DUID into TrafficChain (HDU/LDU lifecycle) and
        // broadcasts to sync_trace + call_boundary + activity feed.
        let traffic_lsm_core = ip_core.clone();
        let traffic_lsm_mgr = traffic_chain.clone();
        let traffic_lsm_stats = traffic_stats.clone();
        let traffic_event_tx = event_tx.clone();
        let traffic_event_log = event_log.clone();
        let traffic_heartbeat_imbe = imbe_forwarder.clone();
        let traffic_boundary_tx = call_boundary_tx.clone();
        let traffic_sync_trace_ring = sync_trace_ring.clone();
        tokio::spawn(async move {
            tracing::info!(
                "traffic LSM heartbeat task started (polling \
                 traffic_lsm_status @ 16 ms)"
            );
            let mut tick = tokio::time::interval(
                std::time::Duration::from_millis(LSM_HEARTBEAT_TICK_MS));
            tick.tick().await;
            let mut total_polls: u64 = 0;
            let mut nid_events: u64 = 0;
            loop {
                tick.tick().await;
                total_polls += 1;
                let (status, nac, duid, pll_dbg, sample_point_dbg, agc_gain, agc_mag) = {
                    let core = traffic_lsm_core.lock().await;
                    let s = core.traffic_lsm_status();
                    let (n, d) = core.traffic_lsm_nid();
                    let (pll, sp) = core.traffic_lsm_debug();
                    let (g, m) = core.traffic_lsm_agc_debug();
                    (s, n, d, pll, sp, g, m)
                };

                if !status.nid_event {
                    continue;
                }
                nid_events += 1;

                use std::sync::atomic::Ordering;
                let sync_trace_call_id = traffic_heartbeat_imbe
                    .current_call_id
                    .load(Ordering::Relaxed);
                if sync_trace_call_id != 0 {
                    let unix_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    crate::services::sync_trace::push(
                        &traffic_sync_trace_ring,
                        crate::services::sync_trace::SyncTraceSample {
                            call_id: sync_trace_call_id,
                            unix_ms,
                            duid,
                            nac,
                            nid_valid: status.nid_valid,
                            bch_busy: status.bch_busy,
                            n_errors: status.n_errors,
                            sync_distance: status.sync_distance,
                            pll_dbg,
                            sample_point_dbg,
                            agc_gain,
                            agc_mag,
                        },
                    );
                }

                if traffic_heartbeat_imbe
                    .current_talkgroup
                    .load(Ordering::Relaxed) == 0
                {
                    continue;
                }

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
                        0x5 => mgr.ldu_received(now, nac, false),
                        0xA => mgr.ldu_received(now, nac, true),
                        0x3 | 0xF => {
                            mgr.tdus_seen += 1;
                            mgr.last_duid = Some(duid);
                            mgr.last_nac = Some(nac);
                        }
                        _ => {
                            mgr.last_duid = Some(duid);
                            mgr.last_nac = Some(nac);
                        }
                    }
                    mgr.current_talkgroup().map(|t| t.0).unwrap_or(0)
                };

                traffic_heartbeat_imbe
                    .last_observed_nac
                    .store(nac, Ordering::Relaxed);
                if locked_tg_snapshot != 0 && duid == 0x0 {
                    let _ = traffic_boundary_tx.send(audio::CallBoundary {
                        kind: audio::CallBoundaryKind::HduStart,
                        nac,
                        talkgroup: Some(locked_tg_snapshot),
                        expected_submit_count: traffic_heartbeat_imbe
                            .frames_submitted
                            .load(Ordering::Relaxed),
                    });
                }

                if locked_tg_snapshot != 0 {
                    let duid_label = match duid {
                        0x0 => "HDU",
                        0x3 => "TDU",
                        0x5 => "LDU1",
                        0xA => "LDU2",
                        0xF => "TDU_LC",
                        _ => "DUID?",
                    };
                    if duid_label != "DUID?" {
                        traffic_event_log.push(
                            crate::services::event_log::LogCategory::Voice,
                            format!(
                                "{} TG={} NAC=0x{:03X}",
                                duid_label, locked_tg_snapshot, nac,
                            ),
                            serde_json::json!({
                                "duid": duid_label,
                                "tg":   locked_tg_snapshot,
                                "nac":  nac,
                            }),
                        );
                    }
                }

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

                let _ = traffic_lsm_stats.lock().await;
            }
        });

        // Phase 2e (2026-04-25): the periodic grant-expiry sweeps
        // (one per control decoder) were removed alongside the
        // decoder's `grants` HashMap. The dashboard's Active Grants
        // panel now reads `state.active_call_snapshot` (mirrored from
        // call_tracker) which drops to None within seconds of TDU /
        // timeout — no zombie 30 s entries to reap.

        (ip_core, ad9361, wideband_iq_capture, sw_demod_enabled, sw_demod_stats)
    };

    // Audio broadcast channel (vocoder -> HTTP/WebSocket).
    // `call_boundary_tx` is created up with `imbe_forwarder` so the
    // traffic-LSM heartbeat task (spawned above) can clone it.
    let audio_tx = audio::audio_channel();

    // Phase 2b (2026-04-25): the recorder subscribes to
    // `CallTrackerEvent` for lifecycle, not raw `CallBoundary`
    // events. Spawn the authority task BEFORE the recorder spawn so
    // we can subscribe in order. grant_stats also subscribes to this
    // channel (spawn below). The tx itself was constructed earlier
    // alongside `call_boundary_tx` so the cfg(linux) grant follower
    // can subscribe too.
    let active_call_snapshot =
        crate::app::grant_follower::new_active_call_shared();
    crate::app::grant_follower::spawn_call_lifecycle(
        call_boundary_tx.clone(),
        audio_tx.clone(),
        call_tracker_tx.clone(),
        imbe_forwarder.clone(),
        active_call_snapshot.clone(),
    );

    // Call recorder: subscribes to audio_tx (for PCM) AND
    // call_tracker_tx (for CallOpen / SourceUpdate / CallClose
    // events). Writes per-call WAVs to /tmp/p25_recordings/. Ring-
    // buffered in RecordingStore for the dashboard.
    let recordings = recorder::new_store();
    let recorder_diag = recorder::new_diag();
    {
        let rx = audio_tx.subscribe();
        let tracker_rx = call_tracker_tx.subscribe();
        let store = recordings.clone();
        let diag = recorder_diag.clone();
        let log = Some(event_log.clone());
        // Share imbe_frames_dropped so each call_finalise event can
        // log the drop-delta during the recording (surfaces IMBE-
        // queue-full events that caused audio loss on specific calls).
        let imbe_drops_handle = imbe_forwarder.imbe_frames_dropped.clone();
        let forwarder_for_recorder = Some(imbe_forwarder.clone());
        tokio::spawn(async move {
            recorder::recorder_task(
                rx, tracker_rx, store, diag, log,
                imbe_drops_handle, forwarder_for_recorder,
            ).await;
        });
    }

    // Modulation auto-detect (SDRTrunk-style). Compares nid_decoded_ok
    // delta between C4FM and LSM decoders once per second, flips
    // `active_modulation` to the winner. Only runs in Auto mode (0);
    // manual overrides via /api/modulation?set=c4fm|lsm freeze it.
    {
        let active_mod = active_modulation.clone();
        let c4fm_decoder = decoder.clone();
        let lsm_decoder_probe = lsm_decoder.clone();
        tokio::spawn(async move {
            let mut last_c4fm: u64 = 0;
            let mut last_lsm: u64 = 0;
            let mut interval = tokio::time::interval(
                std::time::Duration::from_secs(1),
            );
            loop {
                interval.tick().await;
                // Only run when the user has selected Auto (code 0).
                // Manual overrides (1 = C4FM, 2 = LSM) stay put.
                if active_mod.load(std::sync::atomic::Ordering::Relaxed) != 0 {
                    // Reset the deltas so a later switch back to
                    // auto re-probes from a clean baseline.
                    last_c4fm = c4fm_decoder.read().await.nid_decoded_ok;
                    last_lsm = lsm_decoder_probe.read().await.nid_decoded_ok;
                    continue;
                }
                let now_c4fm = c4fm_decoder.read().await.nid_decoded_ok;
                let now_lsm = lsm_decoder_probe.read().await.nid_decoded_ok;
                let d_c4fm = now_c4fm.saturating_sub(last_c4fm);
                let d_lsm = now_lsm.saturating_sub(last_lsm);
                last_c4fm = now_c4fm;
                last_lsm = now_lsm;
                // Need at least one valid NID on the winning side to
                // make a call. Both zero -> no signal -> keep whatever
                // is currently set (or LSM at boot).
                if d_c4fm == 0 && d_lsm == 0 {
                    continue;
                }
                let winner = if d_c4fm > d_lsm { 1u8 } else { 2u8 };
                let current = active_mod.load(
                    std::sync::atomic::Ordering::Relaxed,
                );
                if current != winner && current != 0 {
                    // User is manually set — leave them alone.
                    continue;
                }
                // Store the winner so active_control_decoder() returns
                // the right one. "auto-chose LSM" vs "manually forced
                // LSM" is distinguishable via /api/modulation (raw rates).
                active_mod.store(winner, std::sync::atomic::Ordering::Relaxed);
                tracing::info!(
                    "modulation auto-detect: d_c4fm={d_c4fm} d_lsm={d_lsm} \
                     → {}",
                    if winner == 1 { "C4FM" } else { "LSM" }
                );
            }
        });
    }

    // Audio pacer — gates vocoder→broadcast at exactly 20 ms
    // wall-clock per chunk. Vocoder writes to the mpsc; pacer drains
    // it at native source rate and broadcasts to all subscribers.
    // Without this, post-Tier-A JMBE decode bursts 9 chunks of an LDU
    // batch (180 ms of audio) onto the broadcast in <5 ms, then
    // nothing for 175 ms — the AudioWorklet PLL can't lock on that
    // shape and clients hear ring oscillation. See app::audio_pacer
    // for the full rationale.
    let (pacer_input_tx, pacer_input_rx) = app::audio_pacer::pacer_input_channel();
    app::audio_pacer::spawn_audio_pacer(pacer_input_rx, audio_tx.clone());

    // Vocoder task — reads IMBE batches, decodes via JMBE, pushes
    // AudioChunks to the pacer's mpsc, updates stats atomics.
    // Dedicated OS thread; body in vocoder_task.rs.
    vocoder_task::spawn_vocoder_thread(
        imbe_rx,
        imbe_forwarder.clone(),
        pacer_input_tx,
        event_log.clone(),
    );

    // App state.
    let state = Arc::new(httpd::AppState {
        decoder: decoder.clone(),
        lsm_decoder: lsm_decoder.clone(),
        event_tx,
        #[cfg(target_os = "linux")]
        ip_core,
        #[cfg(target_os = "linux")]
        ad9361,
        // Boot scalars for /api/system. Everything else tunable at
        // runtime lives in the current_* atomics below.
        boot_lo_ppm:       args.lo_ppm,
        boot_control_freq: control_freq,
        current_lo_shift_hz: current_lo_shift_hz.clone(),
        baseline_lo_shift_hz: std::sync::Arc::new(
            std::sync::atomic::AtomicI64::new(
                nco_lo_shift_hz.round() as i64)),
        last_ppm_cal_unix_secs: std::sync::Arc::new(
            std::sync::atomic::AtomicI64::new(0)),
        current_control_freq: std::sync::Arc::new(
            std::sync::atomic::AtomicU64::new(control_freq)),
        current_rx_lo:           current_rx_lo.clone(),
        current_sample_rate_hz:  current_sample_rate_hz.clone(),
        current_preset_idx:      current_preset_idx.clone(),
        center_locked:           center_locked.clone(),
        hdl_lsm: hdl_lsm.clone(),
        irq_stats: irq_stats.clone(),
        traffic_chain: traffic_chain.clone(),
        traffic_stats: traffic_stats.clone(),
        traffic_follower_enabled: traffic_follower_enabled.clone(),
        traffic_lock_freq:        traffic_lock_freq.clone(),
        traffic_lsm_decoder: traffic_lsm_decoder.clone(),
        imbe_forwarder: imbe_forwarder.clone(),
        monitor_list: monitor_list.clone(),
        audio_tx: audio_tx.clone(),
        audio_ws_lag_total: std::sync::Arc::new(
            std::sync::atomic::AtomicU64::new(0),
        ),
        boot_instant: std::time::Instant::now(),
        event_log: event_log.clone(),
        recordings: recordings.clone(),
        recorder_diag: recorder_diag.clone(),
        active_modulation: active_modulation.clone(),
        grant_decode_stats: crate::app::grant_stats::new_ring(),
        enc_grant_decode_stats: crate::app::grant_stats::new_ring(),
        active_call_snapshot: active_call_snapshot.clone(),
        ppm_tracker_ring: std::sync::Arc::new(
            std::sync::Mutex::new(
                std::collections::VecDeque::with_capacity(300))),
        ppm_last_shift_change_ms: std::sync::Arc::new(
            std::sync::atomic::AtomicU64::new(0)),
        // Default: auto-PPM apply ENABLED with a 50 Hz anchor window.
        // 50 Hz ≈ 0.06 ppm at 858 MHz — tight enough to keep the
        // tracker contained near the Stage A peak-find ground truth,
        // loose enough to track genuine thermal drift. The first
        // forced Recalibrate this session sets the anchor centre;
        // before that the tracker stays advisory (can't apply).
        auto_ppm_enabled: std::sync::Arc::new(
            std::sync::atomic::AtomicBool::new(true)),
        auto_ppm_anchor_hz: std::sync::Arc::new(
            std::sync::atomic::AtomicU32::new(50)),
        last_recal_shift_hz: std::sync::Arc::new(
            std::sync::atomic::AtomicI64::new(0)),
        sync_trace_ring: sync_trace_ring.clone(),
        #[cfg(target_os = "linux")]
        wideband_iq_capture: wideband_iq_capture.clone(),
        #[cfg(target_os = "linux")]
        sw_demod_enabled: sw_demod_enabled.clone(),
        #[cfg(target_os = "linux")]
        sw_demod_stats: sw_demod_stats.clone(),
        active_site: {
            // 2026-05-03: hydrate the active site at boot from the
            // last-active marker (or fall back to "clay"). Failure to
            // load is non-fatal: the receiver still functions, the
            // site selector just shows "no site" until the operator
            // picks one.
            let name = crate::services::sites::read_active_site_name();
            let site = match crate::services::sites::load_site(&name) {
                Ok(s) => Some(s),
                Err(e) => {
                    tracing::warn!(
                        "active site '{name}' unavailable at boot: {e}"
                    );
                    None
                }
            };
            Arc::new(tokio::sync::RwLock::new(site))
        },
    });

    // Phase 2b unified call lifecycle: call_tracker is spawned up
    // alongside the recorder (so the recorder can subscribe to its
    // CallTrackerEvent broadcast at construction time). grant_stats
    // also subscribes to the same broadcast — both consumers now
    // share a single source of truth for call identity.
    // See `doc/diagnostics/2026-04-25/UNIFIED_CALL_LIFECYCLE.md`.
    crate::app::grant_stats::spawn_grant_stats_task(
        call_tracker_tx.clone(),
        state.imbe_forwarder.clone(),
        state.grant_decode_stats.clone(),
        state.enc_grant_decode_stats.clone(),
    );

    // 2026-04-26 per-call AGC tracking. Tiny poller updates
    // ImbeForwarder.last_traffic_agc_gain_q97 every 250 ms from the
    // FPGA's traffic LSM AGC debug register. grant_stats reads the
    // atomic at CallClose to record per-call converged gain.
    // 250 ms is fine grain enough to catch the converged value
    // within an LDU pair of the close, no measurable register-bus
    // load. cfg(linux) only — no fpga::IpCore on the host stub.
    //
    // Guard: only sample when the AGC loop is actually enabled.
    // If `agc_enabled` is false, the gain register holds whatever
    // static value was last latched — sampling it pollutes the
    // per-freq cache with bogus values that look like converged
    // gains but aren't (operator-observed 2026-04-26: AGC silently
    // disabled, cache populated with values 5-10× higher than
    // real converged gain). When disabled, write 0 to the atomic;
    // the cache update path in grant_stats already skips q97==0
    // samples.
    // M2B 2026-05-02: per-call traffic AGC poller restored. Tiny
    // poller updates ImbeForwarder.last_traffic_agc_gain_q97 every
    // 250 ms from the FPGA's traffic LSM AGC debug register.
    // grant_stats reads the atomic at CallClose to record per-call
    // converged gain. Guarded on `agc_enabled` — when disabled, the
    // gain register holds a stale latched value (per-freq cache
    // would otherwise be polluted). When disabled we write 0 so
    // the cache update path in grant_stats skips the sample.
    #[cfg(target_os = "linux")]
    {
        let agc_forwarder = state.imbe_forwarder.clone();
        let agc_core = state.ip_core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(
                std::time::Duration::from_millis(250));
            tick.tick().await;
            loop {
                tick.tick().await;
                let core = agc_core.lock().await;
                let (_en, _dma, _dc, agc_enabled) =
                    core.traffic_lsm_control_readback();
                let value = if agc_enabled {
                    let (gain_q97, _mag) = core.traffic_lsm_agc_debug();
                    gain_q97
                } else {
                    0
                };
                drop(core);
                agc_forwarder.last_traffic_agc_gain_q97
                    .store(value,
                           std::sync::atomic::Ordering::Relaxed);
            }
        });
    }

    // Boot-time auto-PPM: wait for system acquisition then run one
    // full stage A + B calibration, persisting the result. Does
    // nothing if persisted calibration was already loaded at boot
    // (in that case we expect fast re-lock on the stored shift).
    #[cfg(target_os = "linux")]
    if ppm_source != "persisted" {
        app::autoppm::spawn_boot_autoppm(state.clone());
    }
    // Periodic fine-tune: every 15 min, if |PLL residual| > 30 Hz,
    // nudge the DDC NCO by the residual. Catches slow crystal
    // drift over temperature without re-running stage A.
    #[cfg(target_os = "linux")]
    app::autoppm::spawn_periodic_fine_tune(state.clone());

    // Start HTTP (and optionally HTTPS). HTTPS unlocks AudioWorklet
    // on the dashboard — browsers only expose it in secure contexts,
    // so http:// to a LAN IP falls back to ScriptProcessorNode on the
    // main thread, contending with refresh/event-log/eye-plot work
    // and causing audible dropouts. Pattern matches maia-httpd.
    let app = httpd::router(state, args.ca_cert.clone());

    let http_addr: std::net::SocketAddr = args.listen.parse()
        .map_err(|e| anyhow::anyhow!("invalid --listen {}: {e}", args.listen))?;
    tracing::info!("Dashboard at http://{}", http_addr);

    // TCP_NODELAY on every accepted connection. Default acceptor
    // leaves Nagle on (40 ms ACK delay), which coalesces the small
    // 320-byte /ws/audio frames and adds avoidable latency to the
    // live audio path. NoDelayAcceptor is a tiny wrapper that calls
    // set_nodelay(true) on each TcpStream.
    use axum_server::accept::NoDelayAcceptor;
    match (args.ssl_cert.as_ref(), args.ssl_key.as_ref()) {
        (Some(cert), Some(key)) => {
            use axum_server::tls_rustls::RustlsConfig;
            let tls = RustlsConfig::from_pem_file(cert, key).await
                .map_err(|e| anyhow::anyhow!(
                    "loading TLS cert/key from {cert:?} / {key:?}: {e}"
                ))?;
            tracing::info!("Dashboard also at https://{}", args.listen_https);
            let http_server = axum_server::bind(http_addr)
                .acceptor(NoDelayAcceptor::new())
                .serve(app.clone().into_make_service());
            let https_server = axum_server::bind_rustls(args.listen_https, tls)
                .map(|rustls| rustls.acceptor(NoDelayAcceptor::new()))
                .serve(app.into_make_service());
            tokio::select! {
                r = http_server  => r?,
                r = https_server => r?,
            };
        }
        _ => {
            // Cert args missing — HTTP-only. Prints why so operators
            // can tell an intentional HTTP-only run (missing args in
            // the init script) from a cert-load failure above.
            if args.ssl_cert.is_some() || args.ssl_key.is_some() {
                tracing::warn!(
                    "--ssl-cert and --ssl-key must both be provided; \
                     running HTTP-only"
                );
            }
            axum_server::bind(http_addr)
                .acceptor(NoDelayAcceptor::new())
                .serve(app.into_make_service())
                .await?;
        }
    }
    Ok(())
}

