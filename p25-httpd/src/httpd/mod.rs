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
//!   - Change 056: `/` serves the web UI — native ES modules under
//!     `ui/` (alongside this file), embedded at compile time by
//!     [`ui_assets`] and served under `/ui/<BUILD_TAG>/...` (see
//!     `api::ui`). The pre-056 single-file dashboard (`/legacy`) was
//!     retired after change 059.
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
    routing::{get, post}, Router,
};
use tokio::sync::{broadcast, RwLock};

use crate::protocol::p25::control_channel::ControlChannelDecoder;

pub mod api;

/// Shared application state
/// Change 073: what the radio learned about one site, kept while
/// another is active.
#[derive(Default)]
pub struct SiteMemory {
    pub encrypted_tgs: std::collections::HashSet<u16>,
    pub grant_map: std::collections::HashMap<(u16, u64), crate::protocol::p25::traffic_chain::GrantMapEntry>,
}

pub struct AppState {
    /// Original Phase 2A C4FM `ControlChannelDecoder`, fed by the C4FM HDL
    /// chain via `dibit_dma`. Retained for diagnostics and as a fallback,
    /// but no longer the primary source for the dashboard's identity /
    /// stats / grants panels (those now read `lsm_decoder` -- see below).
    pub decoder: Arc<RwLock<ControlChannelDecoder>>,
    /// Phase 6E.10 LSM `ControlChannelDecoder`, fed by the HDL LSM chain
    /// via `lsm_dibit_dma`. This is now the source of truth for the
    /// dashboard's System Identity, Decode Stats, Active Grants, and
    /// Frequency Bands panels because LSM simulcast control channels
    /// only decode as garbage through the C4FM chain. Phase 6F.1
    /// dashboard migration -- see doc/changes/024 follow-up notes.
    pub lsm_decoder: Arc<RwLock<ControlChannelDecoder>>,
    // Phase 9 retirement: `iq_lsm_decoder` (Phase 6D software
    // LSM pipeline's TSBK sink) and `lsm_stats` (Phase 6D pipeline
    // runtime stats) were removed here. The HDL LSM chain in
    // `lsm_decoder` and the PL heartbeat `hdl_lsm` runtime below
    // are the single source of truth for the LSM side now. See
    // doc/changes/039 for the rationale.
    pub event_tx: broadcast::Sender<String>,
    #[cfg(target_os = "linux")]
    pub ip_core: Arc<tokio::sync::Mutex<crate::hardware::fpga::IpCore>>,
    /// AD9361 IIO handle for live AGC gain / RSSI readback in /api/stats.
    /// Stateless wrapper around sysfs paths -- safe to share without a lock.
    #[cfg(target_os = "linux")]
    pub ad9361: Arc<crate::hardware::iio::Ad9361>,
    /// Boot-time Pluto crystal calibration (ppm) and the initial
    /// control-channel frequency from the CLI. These don't change at
    /// runtime; they're captured here for `/api/system` reporting and
    /// for the tuning handlers' NCO-offset math.
    pub boot_lo_ppm: f64,
    pub boot_control_freq: u64,
    /// Current DDC NCO shift in Hz, tracking crystal-trim correction.
    /// Boot-initialised from `-boot_lo_ppm * 1e-6 * rx_lo` so the first
    /// read matches the CLI default. Updated by `app::autoppm` each
    /// time a calibration runs; read by `/api/ppm` for display AND by
    /// the `/api/tune` retune path so PPM survives frequency changes.
    pub current_lo_shift_hz:
        std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// Reference shift that the periodic fine-tune task is NOT
    /// allowed to wander more than ±0.2 ppm away from. Set whenever
    /// a full calibration or operator override establishes a new
    /// "known-good" setpoint (boot-time load, /api/ppm_calibrate,
    /// PUT /api/ppm). Fine-tune can only nibble ±0.2 ppm × rx_lo
    /// off this value.
    pub baseline_lo_shift_hz:
        std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// Unix seconds of the last successful auto-PPM calibration, 0 if
    /// never calibrated. Read by `/api/ppm` so dashboards can show
    /// "last calibrated N min ago".
    pub last_ppm_cal_unix_secs:
        std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// Operator-facing tuned frequency in Hz. Boot-initialised from
    /// the CLI `--control-freq`; updated on every successful
    /// `POST /api/tune`. This is the NUMBER A HUMAN TYPES — not the
    /// DDC NCO setpoint (which bakes in PPM correction). Exposed in
    /// `/api/stats` as `radio_freq_hz` so the dashboard tuner widget
    /// can populate its input without the operator re-deriving it.
    pub current_control_freq: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Live RX LO, sample rate, and preset index. Written by the
    /// `/api/preset` and `/api/tune` handlers; read by the grant
    /// follower on every retune and by `/api/stats` / `/api/system`
    /// for display.
    pub current_rx_lo: std::sync::Arc<std::sync::atomic::AtomicI64>,
    pub current_sample_rate_hz:
        std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Index into `ddc_presets::PRESETS` for the live preset. Kept as
    /// an atomic (vs a lock) so the grant follower can read it cheaply.
    pub current_preset_idx:
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Scanner center-lock flag. When true, `/api/tune` refuses to
    /// move the AD9361 LO (only the DDC NCO moves) and returns 409 if
    /// the requested radio frequency falls outside ±(BW/2 − guard) of
    /// the current LO. When false, `/api/tune` auto-recenters the LO
    /// on the nearest 100 kHz step whenever the window would be
    /// exceeded.
    pub center_locked: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Phase 6F.2: PL HDL LSM chain runtime stats, populated by the
    /// HDL LSM heartbeat task. Read by `/api/hdl_lsm`. Single source
    /// of truth for everything the heartbeat task observes about the
    /// FPGA-side LSM chain (registers, NID counts, NAC histogram,
    /// last 32 NID ring buffer).
    pub hdl_lsm: Arc<tokio::sync::Mutex<crate::HdlLsmRuntime>>,
    /// Phase 6F.2: per-source IRQ counters from the InterruptHandler
    /// task. Read by `/api/irq_stats`.
    pub irq_stats: Arc<tokio::sync::Mutex<crate::IrqStats>>,
    /// Traffic-channel singleton, driven by
    /// `app::grant_follower::spawn_grant_follower` consuming the typed
    /// `GrantEvent` mpsc broadcast emitted by the LSM control-channel
    /// decoder. Read by `/api/traffic` to surface state, current
    /// TG/channel/frequency, NCO offset, and retune counters.
    pub traffic_chain:
        Arc<tokio::sync::Mutex<crate::protocol::p25::traffic_chain::TrafficChain>>,
    /// Phase 7A.1: data-side counters for the traffic dibit DMA path,
    /// updated by the traffic dibit reader task in main.rs. Read by
    /// `/api/traffic` alongside the TrafficChain state.
    pub traffic_stats: Arc<tokio::sync::Mutex<crate::TrafficStats>>,
    /// Phase 7A.1: when false, the grant follower task in main.rs
    /// skips its 50 ms poll iteration entirely (no retunes, no
    /// timeouts). Flipped via `GET /api/traffic?follower=on|off` so
    /// the user can take manual control of the traffic DDC NCO +
    /// demod_enable bits without the polling task immediately
    /// overriding them. Default true; process-lifetime only.
    pub traffic_follower_enabled: Arc<std::sync::atomic::AtomicBool>,
    /// 2026-04-24 diagnostic — when true, the grant follower stops
    /// dispatching retunes. The traffic chain stays parked on its
    /// current frequency. Lets an operator measure AGC/PLL/sync
    /// settle behaviour against a known-active traffic channel
    /// without the follower retuning away on the next grant. Toggled
    /// via `POST /api/traffic_lock`.
    pub traffic_lock_freq: Arc<std::sync::atomic::AtomicBool>,
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
    pub monitor_list: Arc<RwLock<crate::services::monitor::MonitorList>>,
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
    pub event_log: Arc<crate::services::event_log::EventLog>,
    /// 2026-04-16: ring of recent call recordings. The recorder
    /// task in main.rs subscribes to audio_tx and populates this.
    /// Consumed by `/api/recordings` (JSON list) and
    /// `/api/recordings/{id}.wav` (file download).
    pub recordings: crate::audio::recorder::RecordingStore,
    /// 2026-04-19: recorder task diagnostics — call-boundary event
    /// counters + lag counts. Visible via `/api/traffic` so we can
    /// see whether Motorola TALK_COMPLETE source stamps are arriving
    /// at the recorder before the matching `ActiveCall` gets
    /// finalised by the grace window.
    pub recorder_diag: crate::audio::recorder::RecorderDiagArc,
    /// 2026-04-16: P25 modulation mode currently driving the
    /// dashboard's primary decoder read path + grant-follower
    /// dispatch. SDRTrunk-style auto-detect: a background task
    /// compares `decoder.nid_decoded_ok` (C4FM) vs
    /// `lsm_decoder.nid_decoded_ok` (LSM) delta every second and
    /// picks the winner. Manual override via `/api/modulation`.
    ///
    /// Encoding:
    ///   0 = Auto (probing; defaults to LSM until first valid NID)
    ///   1 = C4FM (force C4FM control chain)
    ///   2 = LSM  (force LSM simulcast control chain)
    pub active_modulation: Arc<std::sync::atomic::AtomicU8>,
    /// Change 071b: the modulation setting (0 = auto, 1 = C4FM, 2 = LSM);
    /// `active_modulation` is the decoder chosen (1 or 2).
    pub modulation_mode: Arc<std::sync::atomic::AtomicU8>,
    /// Change 071b: control / traffic-chain-1 DDC IQ, one reader each.
    pub control_iq: Arc<crate::app::iq_hub::IqHub>,
    pub traffic_iq: Arc<crate::app::iq_hub::IqHub>,
    /// Change 071b: the software C4FM path's runtime figures.
    pub c4fm_rt: Arc<crate::app::c4fm_task::C4fmRuntime>,
    /// Change 071: who may move the radio (a sweep takes it).
    pub radio_lease: Arc<crate::app::discovery::RadioLease>,
    /// Change 071: the system finder's progress and results.
    pub discovery: crate::app::discovery::SharedDiscovery,
    /// Change 072: the activity history (None: the database did not open).
    pub history: Option<Arc<crate::services::history::HistoryStore>>,
    /// Change 073: what the radio learned about the sites not active
    /// (grant map, encrypted talkgroups); swapped in on a switch.
    pub site_memory: std::sync::Mutex<std::collections::HashMap<String, SiteMemory>>,
    /// Change 074: packet data seen (records, totals per radio).
    pub data: crate::app::data_task::SharedData,
    /// Change 074: the data-only decoder of each traffic chain.
    pub data_decoders: Vec<Arc<RwLock<ControlChannelDecoder>>>,
    /// 2026-04-24: per-grant decode summary ring for **clear /
    /// followed** calls. 2026-04-29: split from the encrypted ring
    /// (below) so heavy ENC GRANT activity (which produces 0-IMBE
    /// synthetic-emit entries) can't push clear-call entries out
    /// of the dashboard ring before recordings can pair by
    /// call_id. Consumed by `/api/grant_decode_stats` (the default
    /// without `?include_enc=1`).
    pub grant_decode_stats: crate::app::grant_stats::GrantStatsRing,
    /// 2026-04-29: per-grant decode summary ring for **encrypted
    /// or not_followed** grants. Smaller cap (50) — operator
    /// visibility into ENC activity without crowding the clear
    /// ring. `/api/grant_decode_stats?include_enc=1` merges this
    /// in. Dashboard checkbox controls the query.
    pub enc_grant_decode_stats: crate::app::grant_stats::GrantStatsRing,

    /// 2026-04-25 Phase 2e: pull-side mirror of the call_tracker's
    /// currently-active call (None when idle). Replaces the long-
    /// lived `ControlChannelDecoder.grants` HashMap as the source of
    /// truth for `/api/grants`, `/api/stats.active_grants`, and the
    /// dashboard's Active Grants panel. Mirrored on every
    /// CallTracker mutation; reads under the std::sync::Mutex are
    /// short-lived (microseconds) so HTTP handlers don't need async.
    pub active_call_snapshot:
        crate::app::grant_follower::ActiveCallShared,
    /// Change 066: the traffic chains that run, lane One first (its
    /// objects are also `traffic_chain` / `traffic_lsm_decoder` /
    /// `imbe_forwarder` / `active_call_snapshot`).
    pub traffic_lanes: Vec<crate::app::traffic_lane::TrafficLane>,

    /// 2026-04-24: shared auto-PPM tracker ring. The sampler task
    /// pushes estimates, the updater reads + clamps + applies. Shared
    /// via AppState so `run_calibration` (and any future code that
    /// changes shift out-of-band) can clear it — stale estimates from
    /// pre-shift-change corrupt the trimmed mean otherwise.
    pub ppm_tracker_ring:
        std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<f64>>>,
    /// 2026-04-24: timestamp (unix ms) of the most recent shift
    /// change — updated by tracker applies and forced calibrations.
    /// Sampler skips sampling for a few seconds after this to let
    /// the Costas PLL reconverge on the new NCO position.
    pub ppm_last_shift_change_ms:
        std::sync::Arc<std::sync::atomic::AtomicU64>,

    /// 2026-04-24: auto-PPM apply gate. When false the tracker still
    /// samples + exposes `tracker_estimate_hz` as advisory, but does
    /// NOT reprogram the NCO. Toggled from the dashboard.
    pub auto_ppm_enabled:
        std::sync::Arc<std::sync::atomic::AtomicBool>,

    /// 2026-04-24: anchor window (Hz). Tracker only applies an estimate
    /// within ±this distance of `last_recal_shift_hz`. Prevents a
    /// biased pll_dbg reading from walking shift away from the
    /// spectrum peak-find's ground-truth position. 0 = no anchor
    /// restriction (legacy behaviour).
    pub auto_ppm_anchor_hz:
        std::sync::Arc<std::sync::atomic::AtomicU32>,

    /// 2026-04-24: snapshot of `current_lo_shift_hz` at the most
    /// recent successful forced recalibration. Used as the anchor
    /// centre for the tracker. 0 until the first recal this session.
    pub last_recal_shift_hz:
        std::sync::Arc<std::sync::atomic::AtomicI64>,

    /// 2026-04-30 post-pacer sync diagnostic ring. The traffic LSM
    /// heartbeat pushes a `SyncTraceSample` on every NID event during
    /// an active call; `/api/recordings/{id}/sync_trace` filters by
    /// `call_id`. Used to localise mid-call sync loss (PLL drift, AGC
    /// overshoot, sync-correlator threshold, symbol-timing slip) per
    /// `project_post_pacer_next_steps.md`.
    pub sync_trace_ring: crate::services::sync_trace::SyncTraceRing,

    /// 2026-05-03: wideband raw-IQ capture handle for the PS-side
    /// software P25 stack. `/api/wideband_iq_capture` POSTs a duration
    /// here to dump N seconds of 8 MSPS IQ to /tmp for offline analysis.
    /// GET returns the current/last capture status.
    #[cfg(target_os = "linux")]
    pub wideband_iq_capture: Arc<crate::app::wideband_iq_task::WidebandIqCaptureState>,

    /// 2026-05-03 Track-2 forensics: on-device dibit ring + wideband
    /// auto-trigger so HDL chain output can be diffed against the SW
    /// oracle without host-side polling losing data. Armed via
    /// `/api/forensics_arm`; auto-captures on every CallOpen/CallClose.
    #[cfg(target_os = "linux")]
    pub forensics: Arc<crate::app::forensics::ForensicsRing>,

    /// 2026-05-03 Stage 2B: live software-demod runtime gate. When true
    /// the `sw_demod_task` feeds dibits into `traffic_lsm_decoder` and
    /// the HDL traffic LSM chain is idled (`set_traffic_lsm_enable=0`).
    /// When false the HDL chain is the active source. Default true.
    #[cfg(target_os = "linux")]
    pub sw_demod_enabled: Arc<std::sync::atomic::AtomicBool>,

    /// 2026-05-03 Stage 2B: cumulative stats for the live software demod.
    /// Read by `/api/sw_demod`.
    #[cfg(target_os = "linux")]
    pub sw_demod_stats: Arc<crate::app::sw_demod_task::SwDemodStats>,

    /// 2026-05-03 dual-DDC pivot: per-site baseline (NAC/WACN/IDEN
    /// bands/CC/cc_position). Hydrated at boot from the overlay file
    /// in `/mnt/data/p25/<active_site>.json` ∪ the repo seed at
    /// `p25-httpd/sites/<active_site>.json`. Mutated by the grant
    /// follower as IDEN_UPDATE TSBKs land + saved to the overlay
    /// path. Read by `/api/site*` handlers + the LO-snap policy in
    /// `httpd::api::tuning`.
    pub active_site: Arc<RwLock<Option<crate::services::sites::Site>>>,
    /// 2026-05-03 seeding bake: shared converged-seed snapshot. None
    /// while the control-chain heartbeat is still warming up
    /// (< MIN_CLEAN_SAMPLES clean NIDs). Some after the first commit;
    /// rolling-window median updated on each subsequent clean NID.
    /// Read by `/api/system` for diagnostic surfacing and by
    /// `app::grant_follower::spawn_grant_follower` to warm-start the
    /// traffic chain on retunes.
    #[cfg(target_os = "linux")]
    pub converged_seeds: crate::app::seed_snapshot::ConvergedSeedsShared,

    /// Change 054: dibit ring delivery state (mode switch, poll
    /// interval, per-ring age / epoch / resync statistics). Backs
    /// `/api/dibit_delivery`.
    pub dibit_delivery: Arc<crate::app::dibit_airtime::DibitDelivery>,

    /// Change 056: persisted operator settings (recording on/off +
    /// retention, TG / unit aliases, monitor list) behind
    /// `/api/ui/settings`. Its `recording` policy is shared with the
    /// recorder task.
    pub ui_settings: Arc<crate::services::ui_settings::SettingsStore>,
    /// Change 070: grants per frequency and the auto-recentre switch, per
    /// site (`services::lo_plan`).
    pub lo_plans: Arc<crate::services::lo_plan::PlanStore>,
    /// Change 056: browsers connected to `/ws/audio` (counted by the
    /// handler; `audio_tx.receiver_count()` also counts the recorder
    /// and the call lifecycle).
    pub audio_ws_listeners: Arc<std::sync::atomic::AtomicUsize>,
    /// Change 056: TSBK rate window for `/api/ui/state` site health.
    pub ui_cc_rate: std::sync::Mutex<crate::app::ui_state::RateWindow>,
    /// Change 057: recording stores (RAM / SD) and the SD writer's
    /// status, shared with the recorder.
    pub rec_storage: Arc<crate::audio::rec_storage::RecordingStorage>,
    /// Change 057: bumped when a closed call's grant summary changes
    /// (its air-time tail was counted after the close); part of
    /// `calls_rev`.
    pub grant_stats_rev: crate::app::grant_stats::GrantStatsRev,
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

    /// Change 071: a 409 for handlers that move the radio while the
    /// system finder has it.
    pub fn radio_busy(&self) -> Option<(axum::http::StatusCode, axum::Json<serde_json::Value>)> {
        (!self.radio_lease.is_normal()).then(|| {
            (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({
                    "ok": false,
                    "error": "the system finder has the radio; try again when the sweep ends",
                })),
            )
        })
    }

    /// Human-readable label for the active modulation.
    pub fn active_modulation_label(&self) -> &'static str {
        match self
            .active_modulation
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            1 => "C4FM",
            _ => "LSM",
        }
    }
}

/// Build the HTTP router.
///
/// If `ca_cert` is `Some`, the referenced PEM is served at `/ca.crt` so
/// browsers can download and install the self-signed CA that signed the
/// HTTPS cert, eliminating the "Your connection is not private" warning.
pub fn router(
    state: Arc<AppState>,
    ca_cert: Option<std::path::PathBuf>,
) -> Router {
    let mut r = Router::new();
    if let Some(ca) = ca_cert {
        // ServeFile streams the file from disk on each hit — cheap,
        // and avoids baking the CA into the binary (per-board certs).
        r = r.route_service("/ca.crt", tower_http::services::ServeFile::new(ca));
    }
    r
        // Change 056: the web UI (ES modules under `ui/`, embedded by
        // `ui_assets`) is served at `/`. The pre-056 single-file
        // dashboard (`/legacy`) was retired after 059.
        .route("/", get(api::ui::get_ui_index))
        .route("/index.html", get(api::ui::get_ui_index))
        .route("/ui/{tag}/{*path}", get(api::ui::get_ui_asset))
        .route("/api/ui/state", get(api::ui::get_ui_state))
        .route("/api/ui/calls", get(api::ui::get_ui_calls))
        .route(
            "/api/ui/settings",
            get(api::ui::get_ui_settings).put(api::ui::put_ui_settings),
        )
        .route("/api/sites", get(api::sites::get_sites))
        .route("/api/sites/{name}", get(api::sites::get_site))
        .route("/api/site", post(api::sites::post_site))
        // Change 070: the receive window against the site's channels.
        .route("/api/site/plan", get(api::site_plan::get_site_plan).put(api::site_plan::put_site_plan))
        .route("/api/site/recentre", post(api::site_plan::post_recentre))
        // Change 071: find local systems.
        .route("/api/discovery", get(api::discovery::get_discovery))
        .route("/api/discovery/scan", post(api::discovery::post_scan))
        .route("/api/discovery/cancel", post(api::discovery::post_cancel))
        .route("/api/discovery/add", post(api::discovery::post_add))
        // Change 072: activity history.
        .route("/api/activity/sites", get(api::activity::get_sites))
        .route("/api/activity/summary", get(api::activity::get_summary))
        .route("/api/activity/talkgroups", get(api::activity::get_talkgroups))
        .route("/api/activity/radios", get(api::activity::get_radios))
        .route("/api/activity/radio/{unit}", get(api::activity::get_radio))
        .route("/api/activity/talkgroup/{tg}", get(api::activity::get_talkgroup))
        .route("/api/activity/series", get(api::activity::get_series))
        .route("/api/activity/calls", get(api::activity::get_calls))
        // Change 074: packet data.
        .route("/api/data", get(api::data::get_data))
        .route("/api/system", get(api::system::get_system))
        .route("/api/sys_health", get(api::system::get_sys_health))
        .route("/api/ps_cores", get(api::system::get_ps_cores))
        .route("/api/pipeline",   get(api::system::get_pipeline))
        .route("/api/freq_health", get(api::system::get_freq_health))
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
        // traffic-chain endpoints have a symmetric counterpart on the
        // control side. 2026-04-18 rename: /api/*_iq_capture{,_aligned}
        // → /api/*_dibit_capture{,_aligned} because those endpoints
        // return dibits (post-demod), not IQ. /api/*_iq_dump is the
        // new endpoint that returns actual post-DDC complex IQ as WAV.
        .route("/api/control_lsm_dibit_dump", get(api::chain::get_control_lsm_dibit_dump))
        .route("/api/control_dibit_capture", get(api::chain::get_control_dibit_capture))
        .route("/api/control_dibit_capture_aligned", get(api::chain::get_control_dibit_capture_aligned))
        .route("/api/control_iq_dump", get(api::chain::get_control_iq_dump))
        // Traffic-chain counterparts (Phase 10-prep, 2026-04-16).
        // Identical response shape to the control-side endpoints,
        // but read from `traffic_lsm_decoder` + traffic HDL regs.
        // Needed for symmetric gain / slicer / sync debugging of
        // the post-retune traffic chain without waiting for a call.
        .route("/api/traffic_lsm_dibit_dump", get(api::chain::get_traffic_lsm_dibit_dump))
        .route("/api/traffic_dibit_capture", get(api::chain::get_traffic_dibit_capture))
        .route("/api/traffic_dibit_capture_aligned", get(api::chain::get_traffic_dibit_capture_aligned))
        .route("/api/traffic_iq_dump", get(api::chain::get_traffic_iq_dump))
        .route("/api/traffic_lsm_control", get(api::chain::get_traffic_lsm_control))
        // M2B 2026-05-02: bin-energy dump for the polyphase channelizer.
        // Lets us empirically diagnose the bin↔freq permutation when
        // the framer can't sync against PS-computed `freq_to_bin`.
        .route("/api/traffic_bins",        get(api::chain::get_traffic_bins))
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
        .route("/api/traffic2", get(api::traffic::get_traffic2))
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
        // 2026-04-22 tuning redesign. /api/reinit is gone; these
        // three replace it. See doc/P25_TUNING_REDESIGN.md.
        //   GET  /api/presets — list every DDC preset.
        //   POST /api/preset  — apply a preset (slow path).
        //   POST /api/tune    — scanner-style radio retune (fast path).
        .route("/api/presets", get(api::tuning::get_presets))
        .route("/api/preset",  post(api::tuning::post_preset))
        .route("/api/tune",    post(api::tuning::post_tune))
        // Auto-PPM calibration. Reads wideband FFT + PLL residual,
        // applies crystal-trim correction to the DDC NCO live.
        .route("/api/ppm",           get(api::tuning::get_ppm)
                                      .put(api::tuning::put_ppm))
        .route("/api/ppm_calibrate", post(api::tuning::post_ppm_calibrate))
        .route("/api/ppm/auto",      post(api::tuning::post_ppm_auto))
        .route("/api/ppm/nudge",     post(api::tuning::post_ppm_nudge))
        // HDL LSM AGC idle-gate threshold. Defaults to 256 (Q1.15);
        // retunable per site without rebaking HDL.
        .route("/api/agc_threshold",
               get(api::tuning::get_agc_threshold)
               .put(api::tuning::put_agc_threshold))
        // Call recording + playback.
        .route("/api/recordings", get(api::history::get_recordings)
                                    .delete(api::history::delete_recordings))
        .route("/api/grant_decode_stats", get(api::history::get_grant_decode_stats))
        .route("/api/recordings/{id}", get(api::history::get_recording_file))
        .route("/api/recordings/{id}/events", get(api::history::get_recording_events))
        .route("/api/recordings/{id}/sync_trace", get(api::history::get_recording_sync_trace))
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
        // Phase 10.7: wideband FFT (pre-DDC, HDL spectrometer) — no
        // PS FFT, just unpack the HDL output.
        .route("/api/spectrum_wide", get(api::debug::get_spectrum_wide))
        // Phase 10.8 (2026-04-23): Anritsu-style modulation metrics
        // from the pre-differential IQ ring (HDL rotate output +
        // per-symbol AGC, taken before the diff-demod/slicer). Feeds
        // the Plots tab deviation panel.
        .route("/api/deviation", get(api::debug::get_deviation))
        // Phase 10.8: symbol-time atan2 histogram scaled to Hz
        // (±600 Hz inner, ±1800 Hz outer). Feeds the Plots tab
        // "distribution" panel.
        .route("/api/distribution", get(api::debug::get_distribution))
        // 2026-05-03: wideband raw IQ capture (PS-side software P25
        // stack stage 1). GET = status snapshot; POST?seconds=N
        // dumps N seconds of 8 MSPS samples to /tmp/p25_iq_captures/.
        .route(
            "/api/wideband_iq_capture",
            get(api::debug::get_wideband_iq_capture)
                .post(api::debug::post_wideband_iq_capture),
        )
        // Stage 2B 2026-05-03: live software demod runtime control.
        .route(
            "/api/sw_demod",
            get(api::debug::get_sw_demod)
                .post(api::debug::post_sw_demod),
        )
        // 2026-05-03 Track-2 forensics: on-device dibit + wideband
        // capture, auto-triggered on every CallOpen/CallClose while
        // armed. See app/forensics.rs.
        .route("/api/forensics_status",
               get(api::forensics::get_forensics_status))
        .route("/api/forensics_arm",
               axum::routing::post(api::forensics::post_forensics_arm))
        .route("/api/forensics_disarm",
               axum::routing::post(api::forensics::post_forensics_disarm))
        // Change 054: dibit ring delivery latency / air-time epoch
        // statistics + runtime mode switch (legacy | poll | airtime).
        .route("/api/dibit_delivery",
               get(api::chain::get_dibit_delivery)
                   .post(api::chain::post_dibit_delivery))
        // Self-describing API catalogue for the dashboard's API tab.
        .route("/api/endpoints", get(api::system::get_endpoints))
        .route("/ws/events", get(api::ws::ws_events))
        .route("/ws/audio", get(api::ws::ws_audio))
        .route("/ws/iq", get(api::ws::ws_iq))
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

pub mod ui_assets;
