//! HTTP server: `AppState`, router wiring, and the embedded dashboard.
//!
//! ## Layout
//!
//! Post-Stage-2 (2026-04-17), this module is deliberately thin:
//!
//!   - [`AppState`] — shared state held across all handlers. Contains
//!     decoder handles, IIO/FPGA handles, broadcast channels, and a
//!     couple of runtime toggles. Handler functions receive `State<Arc<AppState>>`.
//!   - [`router`] — the single `Router` construction. Every route is
//!     registered here as `get(api::<module>::<handler>)`; handler
//!     bodies live in the submodules under [`api`].
//!   - [`index_html`] + `DASHBOARD_HTML` — the `/` route serves the
//!     dashboard. The HTML itself lives in `dashboard.html` (alongside
//!     this file) and is included via `include_str!`.
//!   - [`ts_to_ymd_hms`] — shared wall-clock formatter used by
//!     `api::radio` and a few others. Promoted to `pub(crate)` so
//!     submodules can reach it.
//!
//! ## Authoritative endpoint list
//!
//! At runtime: `GET /api/endpoints` (fed by `ENDPOINT_CATALOGUE` in
//! `api::system`). Human-readable: [`doc/P25_API.md`](../../../doc/P25_API.md).
//! Consumer-contract rules: [`doc/API_CONSUMERS.md`](../../../doc/API_CONSUMERS.md).
//!
//! ## Adding an endpoint
//!
//! 1. Put the handler in the appropriate `api::<module>`.
//! 2. Register it in [`router`].
//! 3. Add an `ENDPOINT_CATALOGUE` entry in `api::system`.
//! 4. Bump `BUILD_TAG` in `main.rs`.
//!
//! See `doc/API_CONSUMERS.md` for the full rules.

use std::sync::Arc;

use axum::{
    response::IntoResponse,
    routing::{get, post}, Router,
};
use tokio::sync::{broadcast, RwLock};

use crate::p25::control_channel::ControlChannelDecoder;

pub mod api;

/// Shared application state
pub struct AppState {
    /// Original Phase 2A C4FM `ControlChannelDecoder`, fed by the C4FM HDL
    /// chain via `dibit_dma`. Retained for diagnostics and as a fallback,
    /// but no longer the primary source for the dashboard's identity /
    /// stats / grants panels (those now read `lsm_decoder` -- see below).
    pub decoder: Arc<RwLock<ControlChannelDecoder>>,
    /// Phase 6E.10 LSM `ControlChannelDecoder`, fed by the HDL LSM chain
    /// via `lsm_dibit_dma`. This is now the source of truth for the
    /// dashboard's System Identity, Decode Stats, Active Grants, and
    /// Frequency Bands panels because the test target (Clay County NAC
    /// 0x8A1) is an LSM simulcast control channel that the C4FM decoder
    /// only ever sees as garbage. Phase 6F.1 dashboard migration --
    /// see doc/changes/024 follow-up notes.
    pub lsm_decoder: Arc<RwLock<ControlChannelDecoder>>,
    // Phase 9 retirement: `iq_lsm_decoder` (Phase 6D software
    // LSM pipeline's TSBK sink) and `lsm_stats` (Phase 6D pipeline
    // runtime stats) were removed here. The HDL LSM chain in
    // `lsm_decoder` and the PL heartbeat `hdl_lsm` runtime below
    // are the single source of truth for the LSM side now. See
    // doc/changes/039 for the rationale.
    pub event_tx: broadcast::Sender<String>,
    #[cfg(target_os = "linux")]
    pub ip_core: Arc<tokio::sync::Mutex<crate::fpga::IpCore>>,
    /// AD9361 IIO handle for live AGC gain / RSSI readback in /api/stats.
    /// Stateless wrapper around sysfs paths -- safe to share without a lock.
    #[cfg(target_os = "linux")]
    pub ad9361: Arc<crate::iio::Ad9361>,
    /// Original main.rs boot-time front-end config (AD9361 + DDC NCO),
    /// captured into AppState at startup so `/api/reinit` can restore
    /// the chip + DDC to the boot state without a board reboot, and
    /// also live-retune individual fields (control_freq, rx_lo,
    /// rf_bandwidth, gain_mode, gain_db) without rebuilding firmware.
    pub boot_rx_lo: u64,
    pub boot_sample_rate: u32,
    pub boot_rf_bandwidth: u32,
    pub boot_control_freq: u64,
    pub boot_lo_ppm: f64,
    pub boot_hardwaregain: f64,
    /// Live RX LO tracking. Initialised from boot_rx_lo and updated by
    /// get_reinit after a successful set_rx_lo_frequency. The grant
    /// follower in main.rs reads this on every retune so its DDC NCO
    /// offset math stays correct when rx_lo is moved mid-session via
    /// /api/reinit?rx_lo=... (fixes the stale-follower_rx_lo bug
    /// flagged in the 2026-04-15 session close).
    pub current_rx_lo: std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// Phase 6F.2: PL HDL LSM chain runtime stats, populated by the
    /// HDL LSM heartbeat task. Read by `/api/hdl_lsm`. Single source
    /// of truth for everything the heartbeat task observes about the
    /// FPGA-side LSM chain (registers, NID counts, NAC histogram,
    /// last 32 NID ring buffer).
    pub hdl_lsm: Arc<tokio::sync::Mutex<crate::HdlLsmRuntime>>,
    /// Phase 6F.2: per-source IRQ counters from the InterruptHandler
    /// task. Read by `/api/irq_stats`.
    pub irq_stats: Arc<tokio::sync::Mutex<crate::IrqStats>>,
    /// Phase 7A.1: traffic-channel grant follower. Singleton, driven by
    /// the 50 ms polling task in main.rs that snapshots
    /// `lsm_decoder.grants` and forwards the newest entry. Read by
    /// `/api/traffic` to surface state, current TG/channel/frequency,
    /// NCO offset, and retune counters. Phase 7H will replace the
    /// singleton with a slot allocator over a channelizer.
    pub traffic_manager:
        Arc<tokio::sync::Mutex<crate::p25::traffic_manager::TrafficManager>>,
    /// Phase 7A.1: data-side counters for the traffic dibit DMA path,
    /// updated by the traffic dibit reader task in main.rs. Read by
    /// `/api/traffic` alongside the TrafficManager state.
    pub traffic_stats: Arc<tokio::sync::Mutex<crate::TrafficStats>>,
    /// Phase 7A.1: when false, the grant follower task in main.rs
    /// skips its 50 ms poll iteration entirely (no retunes, no
    /// timeouts). Flipped via `GET /api/traffic?follower=on|off` so
    /// the user can take manual control of the traffic DDC NCO +
    /// demod_enable bits without the polling task immediately
    /// overriding them. Default true; process-lifetime only.
    pub traffic_follower_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// Phase 7C: fourth `ControlChannelDecoder` instance fed by the
    /// new `traffic_lsm_dibit_dma` ring (Phase 7A.2 HDL chain).
    /// Runs HDU/LDU1/LDU2/TDU/TDU_LC dispatch via its installed
    /// voice handler (the `imbe_forwarder` below). Read by
    /// `/api/traffic` for the per-DUID counters and the cumulative
    /// IMBE frame count.
    pub traffic_lsm_decoder:
        Arc<RwLock<ControlChannelDecoder>>,
    /// Phase 7D: IMBE forwarder that counts events AND pushes raw
    /// frame batches to the vocoder task. Atomic counters for both
    /// extraction stats and vocoder output stats (pcm produced,
    /// errors, encrypted skips).
    pub imbe_forwarder: Arc<crate::ImbeForwarder>,
    /// Phase 7B: talkgroup monitor list. When non-empty, only grants
    /// for TGs in the list are followed. When empty, newest-grant
    /// wins (Phase 7A.1 backward compat).
    pub monitor_list: Arc<RwLock<crate::monitor::MonitorList>>,
    /// Phase 7E: audio broadcast channel. The vocoder task sends
    /// AudioChunks here; HTTP/WebSocket handlers subscribe.
    pub audio_tx: crate::audio::AudioTx,
    /// Cumulative count of `Lagged` events observed by /ws/audio
    /// subscribers since boot. Each increment = one broadcast-channel
    /// overrun where a consumer fell behind and lost chunks (audible
    /// gap on the listener side). Surfaced via /api/stats so the
    /// dashboard can distinguish server-side chunk loss from browser-
    /// side jitter-buffer underruns.
    pub audio_ws_lag_total: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Process start time. Used by /api/stats to report uptime_secs.
    pub boot_instant: std::time::Instant,
    /// Phase 7F.1 (2026-04-14): structured event log ring buffer.
    /// See `src/event_log.rs`. Produced by the follower task, IMBE
    /// forwarder, and vocoder task; consumed by the dashboard's
    /// `/api/log` endpoint.
    pub event_log: Arc<crate::event_log::EventLog>,
    /// 2026-04-16: ring of recent call recordings. The recorder
    /// task in main.rs subscribes to audio_tx and populates this.
    /// Consumed by `/api/recordings` (JSON list) and
    /// `/api/recordings/{id}.wav` (file download).
    pub recordings: crate::recorder::RecordingStore,
    /// 2026-04-16: P25 modulation mode currently driving the
    /// dashboard's primary decoder read path + grant-follower
    /// dispatch. SDRTrunk-style auto-detect: a background task
    /// compares `decoder.nid_decoded_ok` (C4FM) vs
    /// `lsm_decoder.nid_decoded_ok` (LSM) delta every second and
    /// picks the winner. Manual override via `/api/modulation`.
    ///
    /// Encoding:
    ///   0 = Auto (probing; defaults to LSM until first valid NID)
    ///   1 = C4FM (force control chain, e.g. FP&L 935, St Johns 774)
    ///   2 = LSM  (force LSM simulcast chain, e.g. Clay/Duval)
    pub active_modulation: Arc<std::sync::atomic::AtomicU8>,
}

impl AppState {
    /// Returns a reference to the currently-active control-channel
    /// decoder based on the resolved modulation. For Auto mode, picks
    /// LSM until the auto-detect task has winners to report.
    pub fn active_control_decoder(
        &self,
    ) -> &Arc<RwLock<ControlChannelDecoder>> {
        match self
            .active_modulation
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            1 => &self.decoder,
            _ => &self.lsm_decoder,
        }
    }

    /// Human-readable label for the active modulation.
    pub fn active_modulation_label(&self) -> &'static str {
        match self
            .active_modulation
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            1 => "C4FM",
            2 => "LSM",
            _ => "Auto",
        }
    }
}

/// Build the HTTP router
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index_html))
        .route("/api/system", get(api::system::get_system))
        .route("/api/sys_health", get(api::system::get_sys_health))
        .route("/api/grants", get(api::radio::get_grants))
        .route("/api/bands", get(api::radio::get_bands))
        .route("/api/stats", get(api::radio::get_stats))
        // Phase 9: /api/lsm (Phase 6D software pipeline stats) retired.
        // /api/hdl_lsm is the PL-side runtime endpoint now.
        .route("/api/hdl_lsm", get(api::radio::get_hdl_lsm))
        .route("/api/irq_stats", get(api::radio::get_irq_stats))
        .route("/api/decoder_compare", get(api::radio::get_decoder_compare))
        .route("/api/dibit_dump", get(api::chain::get_dibit_dump))
        // 2026-04-16 rename: /api/lsm_* → /api/control_lsm_* so the
        // soon-to-be-added /api/traffic_iq_capture + traffic LSM
        // endpoints have a symmetric counterpart on the control side.
        // Previously the "lsm" prefix was misleading for iq_capture
        // (the IQ ring is post-DDC, before the LSM demod); now each
        // chain's IQ capture is explicitly named by channel role.
        .route("/api/control_lsm_dibit_dump", get(api::chain::get_control_lsm_dibit_dump))
        .route("/api/control_iq_capture", get(api::chain::get_control_iq_capture))
        .route("/api/control_iq_capture_aligned", get(api::chain::get_control_iq_capture_aligned))
        // Traffic-chain counterparts (Phase 10-prep, 2026-04-16).
        // Identical response shape to the control-side endpoints,
        // but read from `traffic_lsm_decoder` + traffic HDL regs.
        // Needed for symmetric gain / slicer / sync debugging of
        // the post-retune traffic chain without waiting for a call.
        .route("/api/traffic_lsm_dibit_dump", get(api::chain::get_traffic_lsm_dibit_dump))
        .route("/api/traffic_iq_capture", get(api::chain::get_traffic_iq_capture))
        .route("/api/traffic_iq_capture_aligned", get(api::chain::get_traffic_iq_capture_aligned))
        .route("/api/traffic_lsm_control", get(api::chain::get_traffic_lsm_control))
        // Phase 10-prep: live AD9361 RX gain knob. Previously only
        // reachable via /api/reinit (which rewrites everything);
        // having a dedicated read/write lets us A/B gain during
        // decode debug without disturbing LO / BW / DDC.
        .route("/api/rx_gain", get(api::tuning::get_rx_gain).put(api::tuning::put_rx_gain))
        // Grant frequency map (Phase 10-prep). Every grant observed
        // on the control channel, keyed by (tg, freq), with seen-
        // count, last-seen timestamp, and encryption history. Used
        // by the scanner-mode UI + future LO auto-center.
        .route("/api/grant_map", get(api::talkgroups::get_grant_map))
        .route("/api/tsbk_opcodes", get(api::history::get_tsbk_opcodes))
        .route("/api/recent_tsbks", get(api::history::get_recent_tsbks))
        // Phase 6F.7 testing knobs. Both endpoints accept GET with
        // query params so they work from a plain curl / browser bar
        // without -X PUT / -X POST. The PUT/POST aliases are kept for
        // anyone who wants HTTP-method-correct calls.
        .route("/api/sync_tune", get(api::tuning::get_sync_tune).put(api::tuning::put_sync_tune))
        .route(
            "/api/decoder_reset",
            get(api::tuning::get_decoder_reset).post(api::tuning::post_decoder_reset),
        )
        // Phase 6G.2: runtime read/write of the control-chain
        // `lsm_control` HDL register (lsm_enable, lsm_dibit_dma_enable,
        // lsm_dc_block_enable). 2026-04-16 rename: was /api/lsm_control;
        // now /api/control_lsm_control so the traffic-side counterpart
        // /api/traffic_lsm_control has a symmetric sibling. The HDL
        // register name ("lsm_control") is unchanged.
        .route("/api/control_lsm_control", get(api::chain::get_control_lsm_control))
        // Phase 7A.1: traffic-channel grant follower state + dibit
        // counters. Read-only diagnostic surface for the singleton
        // voice channel scaffold; will gain monitor-list write
        // operations in Phase 7B.
        .route("/api/traffic", get(api::traffic::get_traffic))
        .route("/api/monitor", get(api::talkgroups::get_monitor).put(api::talkgroups::put_monitor))
        .route("/api/audio", get(api::traffic::get_audio))
        .route("/api/imbe_dump", get(api::traffic::get_imbe_dump))
        .route("/api/audio_test", get(api::traffic::get_audio_test))
        .route("/api/log", get(api::history::get_event_log))
        .route("/api/nid_capture", get(api::chain::get_nid_capture))
        .route("/api/bch_t", get(api::tuning::get_bch_t).put(api::tuning::put_bch_t))
        .route(
            "/api/encrypted_tgs",
            get(api::talkgroups::get_encrypted_tgs).put(api::talkgroups::put_encrypted_tgs),
        )
        .route("/api/aliases", get(api::talkgroups::get_aliases).put(api::talkgroups::put_aliases))
        // Runtime front-end re-init + live retune. Default (no params)
        // restores the main.rs boot values captured in AppState.
        // Optional query params override individual fields for this
        // call only, so we can retune the control channel, change
        // AD9361 gain/BW/SR, or move the RX LO live without a Tezuka
        // rebuild + flash. Primary recovery path when anything has
        // clobbered AD9361 / DDC state.
        .route("/api/reinit", get(api::tuning::get_reinit))
        // Call recording + playback.
        .route("/api/recordings", get(api::history::get_recordings))
        .route("/api/recordings/{id}", get(api::history::get_recording_file))
        // Modulation selector (C4FM / LSM / Auto). SDRTrunk-style.
        .route("/api/modulation", get(api::tuning::get_modulation).put(api::tuning::put_modulation))
        // Browser-pushed wall-clock sync. Zero-infra alternative to
        // NTP for boards on isolated networks (RNDIS-over-USB, air-
        // gapped labs). Dashboard auto-posts Date.now() on load.
        .route("/api/set_time", post(api::system::post_set_time))
        // Narrowband software FFT (2026-04-16, Option A). Runs on the
        // Zynq ARM over the existing post-DDC IQ ring for one chain
        // at a time. See src/spectrum.rs. Wideband view (pre-DDC, 8
        // MSPS) deferred to a future HDL bake.
        .route("/api/spectrum", get(api::debug::get_spectrum))
        // Constellation scatter for the Debug tab — reuses the
        // retired Phase 6D `lsm::demod` software port to extract
        // post-PLL symbol-time (I, Q) points from the same iq_dma
        // rings the /api/spectrum endpoint reads.
        .route("/api/constellation", get(api::debug::get_constellation))
        // Self-describing API catalogue for the dashboard's API tab.
        .route("/api/endpoints", get(api::system::get_endpoints))
        .route("/ws/events", get(api::ws::ws_events))
        .route("/ws/audio", get(api::ws::ws_audio))
        .with_state(state)
}

/// Convert Unix epoch seconds (UTC) to (year, month, day, h, m, s).
/// Proleptic Gregorian, matches chrono's naive conversion. Used only
/// by /api/stats so pulling chrono in just for this isn't worth it.
fn ts_to_ymd_hms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    let s = rem % 60;
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = (y + if mo <= 2 { 1 } else { 0 }) as i32;
    (year, mo, d, h, m, s)
}

async fn index_html() -> impl IntoResponse {
    axum::response::Html(DASHBOARD_HTML)
}

const DASHBOARD_HTML: &str = include_str!("dashboard.html");
