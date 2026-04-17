//! HTTP server and REST API
//!
//! Endpoints:
//! - GET  /              -> Dashboard SPA (embedded HTML)
//! - GET  /api/system    -> System identity
//! - GET  /api/grants    -> Active voice grants
//! - GET  /api/bands     -> Frequency band table
//! - GET  /api/stats     -> Decoder statistics
//! - GET  /api/lsm       -> LSM pipeline runtime stats (Phase 6D)
//! - GET  /api/aliases   -> Talkgroup alias map
//! - PUT  /api/aliases   -> Update alias map
//! - WS   /ws/events     -> Real-time TSBK event stream

use std::sync::Arc;

use axum::{
    extract::{ws::WebSocket, Query, State, WebSocketUpgrade},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use tokio::sync::{broadcast, RwLock};

use crate::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, SYNC_THRESHOLD,
};
use p25_json::*;

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
        .route("/api/system", get(get_system))
        .route("/api/grants", get(get_grants))
        .route("/api/bands", get(get_bands))
        .route("/api/stats", get(get_stats))
        // Phase 9: /api/lsm (Phase 6D software pipeline stats) retired.
        // /api/hdl_lsm is the PL-side runtime endpoint now.
        .route("/api/hdl_lsm", get(get_hdl_lsm))
        .route("/api/irq_stats", get(get_irq_stats))
        .route("/api/decoder_compare", get(get_decoder_compare))
        .route("/api/dibit_dump", get(get_dibit_dump))
        // 2026-04-16 rename: /api/lsm_* → /api/control_lsm_* so the
        // soon-to-be-added /api/traffic_iq_capture + traffic LSM
        // endpoints have a symmetric counterpart on the control side.
        // Previously the "lsm" prefix was misleading for iq_capture
        // (the IQ ring is post-DDC, before the LSM demod); now each
        // chain's IQ capture is explicitly named by channel role.
        .route("/api/control_lsm_dibit_dump", get(get_control_lsm_dibit_dump))
        .route("/api/control_iq_capture", get(get_control_iq_capture))
        .route("/api/control_iq_capture_aligned", get(get_control_iq_capture_aligned))
        // Traffic-chain counterparts (Phase 10-prep, 2026-04-16).
        // Identical response shape to the control-side endpoints,
        // but read from `traffic_lsm_decoder` + traffic HDL regs.
        // Needed for symmetric gain / slicer / sync debugging of
        // the post-retune traffic chain without waiting for a call.
        .route("/api/traffic_lsm_dibit_dump", get(get_traffic_lsm_dibit_dump))
        .route("/api/traffic_iq_capture", get(get_traffic_iq_capture))
        .route("/api/traffic_iq_capture_aligned", get(get_traffic_iq_capture_aligned))
        .route("/api/traffic_lsm_control", get(get_traffic_lsm_control))
        // Phase 10-prep: live AD9361 RX gain knob. Previously only
        // reachable via /api/reinit (which rewrites everything);
        // having a dedicated read/write lets us A/B gain during
        // decode debug without disturbing LO / BW / DDC.
        .route("/api/rx_gain", get(get_rx_gain).put(put_rx_gain))
        // Grant frequency map (Phase 10-prep). Every grant observed
        // on the control channel, keyed by (tg, freq), with seen-
        // count, last-seen timestamp, and encryption history. Used
        // by the scanner-mode UI + future LO auto-center.
        .route("/api/grant_map", get(get_grant_map))
        .route("/api/tsbk_opcodes", get(get_tsbk_opcodes))
        .route("/api/recent_tsbks", get(get_recent_tsbks))
        // Phase 6F.7 testing knobs. Both endpoints accept GET with
        // query params so they work from a plain curl / browser bar
        // without -X PUT / -X POST. The PUT/POST aliases are kept for
        // anyone who wants HTTP-method-correct calls.
        .route("/api/sync_tune", get(get_sync_tune).put(put_sync_tune))
        .route(
            "/api/decoder_reset",
            get(get_decoder_reset).post(post_decoder_reset),
        )
        // Phase 6G.2: runtime read/write of the control-chain
        // `lsm_control` HDL register (lsm_enable, lsm_dibit_dma_enable,
        // lsm_dc_block_enable). 2026-04-16 rename: was /api/lsm_control;
        // now /api/control_lsm_control so the traffic-side counterpart
        // /api/traffic_lsm_control has a symmetric sibling. The HDL
        // register name ("lsm_control") is unchanged.
        .route("/api/control_lsm_control", get(get_control_lsm_control))
        // Phase 7A.1: traffic-channel grant follower state + dibit
        // counters. Read-only diagnostic surface for the singleton
        // voice channel scaffold; will gain monitor-list write
        // operations in Phase 7B.
        .route("/api/traffic", get(get_traffic))
        .route("/api/monitor", get(get_monitor).put(put_monitor))
        .route("/api/audio", get(get_audio))
        .route("/api/imbe_dump", get(get_imbe_dump))
        .route("/api/audio_test", get(get_audio_test))
        .route("/api/log", get(get_event_log))
        .route("/api/nid_capture", get(get_nid_capture))
        .route("/api/bch_t", get(get_bch_t).put(put_bch_t))
        .route(
            "/api/encrypted_tgs",
            get(get_encrypted_tgs).put(put_encrypted_tgs),
        )
        .route("/api/aliases", get(get_aliases).put(put_aliases))
        // Runtime front-end re-init + live retune. Default (no params)
        // restores the main.rs boot values captured in AppState.
        // Optional query params override individual fields for this
        // call only, so we can retune the control channel, change
        // AD9361 gain/BW/SR, or move the RX LO live without a Tezuka
        // rebuild + flash. Primary recovery path when anything has
        // clobbered AD9361 / DDC state.
        .route("/api/reinit", get(get_reinit))
        // Call recording + playback.
        .route("/api/recordings", get(get_recordings))
        .route("/api/recordings/{id}", get(get_recording_file))
        // Modulation selector (C4FM / LSM / Auto). SDRTrunk-style.
        .route("/api/modulation", get(get_modulation).put(put_modulation))
        // Browser-pushed wall-clock sync. Zero-infra alternative to
        // NTP for boards on isolated networks (RNDIS-over-USB, air-
        // gapped labs). Dashboard auto-posts Date.now() on load.
        .route("/api/set_time", post(post_set_time))
        // Narrowband software FFT (2026-04-16, Option A). Runs on the
        // Zynq ARM over the existing post-DDC IQ ring for one chain
        // at a time. See src/spectrum.rs. Wideband view (pre-DDC, 8
        // MSPS) deferred to a future HDL bake.
        .route("/api/spectrum", get(get_spectrum))
        // Constellation scatter for the Debug tab — reuses the
        // retired Phase 6D `lsm::demod` software port to extract
        // post-PLL symbol-time (I, Q) points from the same iq_dma
        // rings the /api/spectrum endpoint reads.
        .route("/api/constellation", get(get_constellation))
        // Self-describing API catalogue for the dashboard's API tab.
        .route("/api/endpoints", get(get_endpoints))
        .route("/ws/events", get(ws_events))
        .route("/ws/audio", get(ws_audio))
        .with_state(state)
}

/// Runtime front-end re-init + live retune handler.
///
/// Re-runs the main.rs boot init sequence for BOTH the AD9361 IIO
/// device (rx_lo, sample_rate, rf_bandwidth, gain_control_mode,
/// hardwaregain) AND the HDL control DDC NCO (control_freq →
/// nco_offset), without a board reboot.
///
/// With no query params, restores the exact boot defaults captured in
/// `AppState` at startup. Any of the following optional query params
/// overrides the corresponding field for this call only:
///
/// - `rx_lo`          u64 Hz  — AD9361 RX LO frequency
/// - `control_freq`   u64 Hz  — desired control-channel center frequency
/// - `sample_rate`    u32 Hz  — AD9361 sampling frequency
/// - `rf_bandwidth`   u32 Hz  — AD9361 analog front-end bandwidth
/// - `gain_mode`      str     — `manual|fast_attack|slow_attack|hybrid`
/// - `gain_db`        f64 dB  — manual gain value (only meaningful when
///                              gain_mode=manual; written after mode
///                              switch so the mode change doesn't clobber it)
///
/// The DDC NCO is always recomputed as
/// `control_freq - rx_lo + (-lo_ppm * 1e-6 * rx_lo)` (matching
/// main.rs line ~339) and written via `ip_core.set_ddc_frequency`.
///
/// Examples:
/// ```text
/// # Restore boot defaults (recovery after clobber):
/// curl http://192.168.2.1:8080/api/reinit
///
/// # Try fast-attack AGC at boot BW / freq:
/// curl 'http://192.168.2.1:8080/api/reinit?gain_mode=fast_attack'
///
/// # Move RX LO up 2 MHz and let NCO compensate:
/// curl 'http://192.168.2.1:8080/api/reinit?rx_lo=862500000'
///
/// # Retune to a different control channel entirely:
/// curl 'http://192.168.2.1:8080/api/reinit?control_freq=858237500'
/// ```
#[cfg(target_os = "linux")]
async fn get_reinit(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let rx_lo = params
        .get("rx_lo")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(state.boot_rx_lo);
    let control_freq = params
        .get("control_freq")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(state.boot_control_freq);
    let sr = params
        .get("sample_rate")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(state.boot_sample_rate);
    let bw = params
        .get("rf_bandwidth")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(state.boot_rf_bandwidth);
    // Default gain_mode follows the boot config: if main.rs set a
    // manual hardwaregain, reinit with no params should restore
    // Manual+boot_hardwaregain, not fall back to slow_attack.
    let gm_str = params
        .get("gain_mode")
        .map(String::as_str)
        .unwrap_or("manual");
    let gain_db = params
        .get("gain_db")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(state.boot_hardwaregain);
    let gain_mode = match gm_str {
        "manual" => crate::iio::GainMode::Manual,
        "fast_attack" => crate::iio::GainMode::FastAttack,
        "slow_attack" => crate::iio::GainMode::SlowAttack,
        "hybrid" => crate::iio::GainMode::Hybrid,
        other => {
            return Json(serde_json::json!({
                "ok": false,
                "error": format!(
                    "unknown gain_mode: '{other}'; expected manual|fast_attack|slow_attack|hybrid"
                ),
            }));
        }
    };

    // DDC NCO offset: same math as main.rs boot path. ppm correction
    // shifts the NCO by -ppm * 1e-6 * rx_lo so a Pluto crystal error
    // cancels out at the DDC mixer.
    let nco_lo_shift_hz = -state.boot_lo_ppm * 1e-6 * rx_lo as f64;
    let nco_offset_hz = control_freq as f64 - rx_lo as f64 + nco_lo_shift_hz;

    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    match state.ad9361.set_rx_lo_frequency(rx_lo).await {
        Ok(_) => {
            // Publish the live rx_lo so the grant follower's retune
            // math picks it up on the next grant (2026-04-16 stale
            // follower_rx_lo fix).
            state.current_rx_lo.store(
                rx_lo as i64,
                std::sync::atomic::Ordering::Relaxed,
            );
            applied.push(format!("rx_lo={rx_lo}"));
        }
        Err(e) => errors.push(format!("rx_lo: {e}")),
    }
    match state.ad9361.set_sampling_frequency(sr).await {
        Ok(_) => applied.push(format!("sampling_frequency={sr}")),
        Err(e) => errors.push(format!("sampling_frequency: {e}")),
    }
    match state.ad9361.set_rx_rf_bandwidth(bw).await {
        Ok(_) => applied.push(format!("rf_bandwidth={bw}")),
        Err(e) => errors.push(format!("rf_bandwidth: {e}")),
    }
    match state.ad9361.set_rx_gain_mode(gain_mode).await {
        Ok(_) => applied.push(format!("gain_control_mode={gm_str}")),
        Err(e) => errors.push(format!("gain_control_mode: {e}")),
    }
    // Only write hardwaregain in manual mode. In AGC modes the chip
    // would immediately override anything we wrote.
    if matches!(gain_mode, crate::iio::GainMode::Manual) {
        match state.ad9361.set_rx_gain(gain_db).await {
            Ok(_) => applied.push(format!("hardwaregain={gain_db} dB")),
            Err(e) => errors.push(format!("hardwaregain: {e}")),
        }
    }

    // DDC NCO is a synchronous FPGA register write, but ip_core is
    // behind an async Mutex to serialize register-bank access with
    // the rest of the code.
    {
        let core = state.ip_core.lock().await;
        match core.set_ddc_frequency(nco_offset_hz, sr as f64) {
            Ok(_) => applied.push(format!(
                "ddc_nco_offset={:.0} (control_freq={control_freq})",
                nco_offset_hz
            )),
            Err(e) => errors.push(format!("ddc_nco_offset: {e}")),
        }
    }

    let readback_gain = state.ad9361.get_rx_gain().await.ok();
    let readback_rssi = state.ad9361.get_rx_rssi().await.ok();

    Json(serde_json::json!({
        "ok": errors.is_empty(),
        "applied": applied,
        "errors": errors,
        "requested": {
            "rx_lo":              rx_lo,
            "control_freq":       control_freq,
            "sample_rate":        sr,
            "rf_bandwidth":       bw,
            "gain_control_mode":  gm_str,
            "ddc_nco_offset_hz":  nco_offset_hz,
        },
        "boot_defaults": {
            "rx_lo":              state.boot_rx_lo,
            "control_freq":       state.boot_control_freq,
            "sample_rate":        state.boot_sample_rate,
            "rf_bandwidth":       state.boot_rf_bandwidth,
            "lo_ppm":             state.boot_lo_ppm,
            "gain_control_mode":  "manual",
            "hardwaregain":       state.boot_hardwaregain,
        },
        "readback": {
            "hardwaregain_db": readback_gain,
            "rssi_db":         readback_rssi,
        },
        "note": "Re-runs the main.rs boot front-end init for AD9361 + DDC NCO. Defaults restore boot config; query params override individual fields for live retuning without a Tezuka rebuild. Use as recovery path after anything clobbers AD9361 or DDC state.",
    }))
}

#[cfg(not(target_os = "linux"))]
async fn get_reinit(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "front-end re-init is only available on the target (linux/arm)",
    }))
}

/// `GET /api/recordings`
///
/// Returns the ring of recent call recordings, newest first. Each
/// entry has {id, talkgroup, started_unix_ms, duration_ms,
/// size_bytes}. Download via `/api/recordings/{id}.wav` or
/// `/api/recordings/{id}`.
async fn get_recordings(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let ring = state.recordings.lock().await;
    let items: Vec<_> = ring.iter().rev().cloned().collect();
    Json(serde_json::json!({
        "count": items.len(),
        "max": crate::recorder::MAX_RECORDINGS,
        "items": items,
    }))
}

/// `GET /api/recordings/{id}`
///
/// Streams the WAV file for a recording by id. Trailing `.wav` in
/// the path is tolerated (strip it). Returns 404 if the id isn't in
/// the current ring (evicted or never existed).
async fn get_recording_file(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id_str): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    // Strip optional .wav suffix so both `/api/recordings/123` and
    // `/api/recordings/123.wav` work.
    let id_clean = id_str.trim_end_matches(".wav");
    let id: u64 = match id_clean.parse() {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("bad id '{id_str}'"),
            )
                .into_response();
        }
    };

    let path = {
        let ring = state.recordings.lock().await;
        ring.iter().find(|e| e.id == id).map(|e| e.path.clone())
    };
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "recording not found").into_response();
    };

    // Simple blocking file read — WAVs are at most a few MB and
    // tmpfs-backed. Avoid axum's Body::from_stream machinery.
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("file read failed: {e}"),
            )
                .into_response();
        }
    };
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("recording.wav")
        .to_string();
    let total_len = bytes.len() as u64;

    // Phase 10-prep: HTTP Range support. Without this, the HTML5
    // <audio> element in the dashboard stutters or restarts mid-
    // playback -- it issues `Range: bytes=0-` probes to test for
    // seek capability, gets 200 OK with the full body, and re-
    // interprets the re-send as a stream restart. Implementing
    // minimal single-range support (206 Partial Content) makes
    // <audio> happy. Download (`<a href download>`) still works
    // because the download path doesn't issue Range requests.
    //
    // We intentionally parse ONLY `bytes=<start>-<end?>` (single
    // range, no multipart) since that covers every browser we care
    // about. Malformed ranges fall back to 200 OK with the full body.
    let range_hdr = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("bytes="))
        .and_then(|s| s.split_once('-'))
        .and_then(|(start, end)| {
            let start: u64 = start.parse().ok()?;
            let end: u64 = if end.is_empty() {
                total_len.saturating_sub(1)
            } else {
                end.parse().ok()?
            };
            if start > end || start >= total_len {
                return None;
            }
            let end = end.min(total_len - 1);
            Some((start, end))
        });

    let (status, start, end) = match range_hdr {
        Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e),
        None => (StatusCode::OK, 0u64, total_len - 1),
    };

    let body: Vec<u8> = if status == StatusCode::PARTIAL_CONTENT {
        bytes[start as usize..=end as usize].to_vec()
    } else {
        bytes
    };
    let content_length = body.len() as u64;

    let mut builder = axum::response::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(
            header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{filename}\""),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, content_length);

    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total_len}"),
        );
    }

    match builder.body(axum::body::Body::from(body)) {
        Ok(resp) => resp,
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("response build failed: {e}"),
        )
            .into_response(),
    }
}

/// `GET /api/spectrum?chain=control|traffic`
///
/// Runs a 4096-point FFT over the most recent 65.5 ms of the
/// selected post-DDC IQ ring. Returns magnitude in dBFS, fftshifted
/// so bin 0 is the most-negative frequency (-31.25 kHz relative to
/// the chain's DDC center). `chain` defaults to `control`.
///
/// The traffic chain requires the bake #2 bitstream flashed; on
/// older binaries the endpoint returns an error explaining the
/// missing UIO device. The control chain works on any bitstream
/// that has the Phase 6C `iq_dma` ring.
#[cfg(target_os = "linux")]
async fn get_spectrum(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let chain = params
        .get("chain")
        .map(String::as_str)
        .unwrap_or("control");

    // Pull buffers from the requested ring. read_*_buffers() is a
    // rolling-window reader — call it once to advance our bookkeeping,
    // then concatenate what we got. If the first call returns empty
    // (fresh session or we're mid-burst), retry once after a short
    // sleep so the first spectrum request after boot doesn't just 404.
    let bytes: Vec<u8> = {
        let mut core = state.ip_core.lock().await;
        let mut acc: Vec<u8> = Vec::new();
        for _retry in 0..2 {
            let bufs: Vec<&[u8]> = match chain {
                "control" => core.read_iq_buffers(),
                "traffic" => core.read_traffic_iq_buffers(),
                other => {
                    return Json(serde_json::json!({
                        "ok": false,
                        "error": format!(
                            "unknown chain '{other}'; expected control|traffic"
                        ),
                    }));
                }
            };
            if !bufs.is_empty() {
                for b in bufs {
                    acc.extend_from_slice(b);
                }
                break;
            }
            // Drop the lock between retries so the DMA can make
            // progress on the producer side.
            drop(core);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            core = state.ip_core.lock().await;
        }
        acc
    };

    let Some(snap) = crate::spectrum::spectrum_from_bytes(&bytes) else {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "not enough IQ samples for FFT_SIZE={} on chain={} \
                 ({} samples available)",
                crate::spectrum::FFT_SIZE,
                chain,
                bytes.len() / 4,
            ),
        }));
    };

    // Center frequency for the display axis. Control chain =
    // boot_control_freq (matches what /api/stats reports as the
    // current tuned control center); traffic chain = RX LO +
    // TrafficManager.last_offset_hz (the follower's per-call NCO).
    // Fall back to RX LO if the traffic chain hasn't been retuned.
    let rx_lo = state
        .ad9361
        .get_rx_lo_frequency()
        .await
        .unwrap_or(state.boot_rx_lo) as f64;
    let center_hz: f64 = match chain {
        "control" => state.boot_control_freq as f64,
        "traffic" => {
            let mgr = state.traffic_manager.lock().await;
            rx_lo + mgr.last_offset_hz as f64
        }
        _ => rx_lo,
    };

    Json(serde_json::json!({
        "ok":              true,
        "chain":           chain,
        "center_hz":       center_hz,
        "sample_rate_hz":  snap.sample_rate_hz,
        "fft_size":        snap.mag_db.len(),
        "mag_db":          snap.mag_db,
    }))
}

#[cfg(not(target_os = "linux"))]
async fn get_spectrum(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "spectrum only available on the target (linux/arm)",
    }))
}

/// `GET /api/constellation?chain=control|traffic`
///
/// Runs the retired Phase 6D software LSM demod pipeline
/// (`lsm::demod::demod_lsm`) over a fresh IQ buffer and returns the
/// post-PLL soft-symbol (I, Q) points. The dashboard renders these
/// as a 4-quadrant scatter plot so the user can visually inspect
/// phase noise, radial compression, and quadrant bias on either
/// chain. Particularly useful on the traffic chain for diagnosing
/// robotic-audio / marginal-decode symptoms from scatter cloud
/// shape.
#[cfg(target_os = "linux")]
async fn get_constellation(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let chain = params
        .get("chain")
        .map(String::as_str)
        .unwrap_or("traffic");

    // Same 2-try read pattern as /api/spectrum: drop the lock
    // between retries so the producer can push new samples.
    let bytes: Vec<u8> = {
        let mut core = state.ip_core.lock().await;
        let mut acc: Vec<u8> = Vec::new();
        for _retry in 0..2 {
            let bufs: Vec<&[u8]> = match chain {
                "control" => core.read_iq_buffers(),
                "traffic" => core.read_traffic_iq_buffers(),
                other => {
                    return Json(serde_json::json!({
                        "ok": false,
                        "error": format!(
                            "unknown chain '{other}'; expected control|traffic"
                        ),
                    }));
                }
            };
            if !bufs.is_empty() {
                for b in bufs {
                    acc.extend_from_slice(b);
                }
                break;
            }
            drop(core);
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            core = state.ip_core.lock().await;
        }
        acc
    };

    if bytes.len() < 4 * 512 {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "not enough IQ samples for constellation on chain={} \
                 ({} samples available)",
                chain,
                bytes.len() / 4,
            ),
        }));
    }

    // Decode interleaved-IQ bytes → Complex32 vec. 2048 samples ≈
    // 33 ms at 62.5 kSPS, which produces ~157 symbols at 4800 sym/s.
    // Enough for a dense scatter without over-spending on the JSON
    // payload size.
    let n_samples = std::cmp::min(bytes.len() / 4, 2048);
    let start = bytes.len() - n_samples * 4;
    let mut iq: Vec<crate::lsm::Complex32> = Vec::with_capacity(n_samples);
    for chunk in bytes[start..].chunks_exact(4) {
        let r = i16::from_le_bytes([chunk[0], chunk[1]]) as f32;
        let i = i16::from_le_bytes([chunk[2], chunk[3]]) as f32;
        iq.push(crate::lsm::Complex32::new(r, i));
    }

    // Run the software LSM demod pipeline. `demod_lsm` handles the
    // /2 decimator + LPF + RRC + timing recovery + PLL rotate +
    // differential demod steps internally; soft_symbols are the
    // post-PLL decision-time (I, Q) points we plot. sample_rate is
    // the post-DDC 62.5 kSPS.
    let result = crate::lsm::demod::demod_lsm(
        &iq,
        crate::spectrum::SAMPLE_RATE_HZ,
    );

    // Keep the payload small: return up to 512 most recent points
    // as packed arrays (two parallel f32 vecs + a clip count).
    let take = std::cmp::min(result.soft_symbols.len(), 512);
    let start_sym = result.soft_symbols.len() - take;
    let (i_arr, q_arr): (Vec<f32>, Vec<f32>) = result
        .soft_symbols[start_sym..]
        .iter()
        .map(|s| (s.re, s.im))
        .unzip();

    Json(serde_json::json!({
        "ok":         true,
        "chain":      chain,
        "count":      take,
        "i":          i_arr,
        "q":          q_arr,
        "pll_final":  result.pll_trace.last().copied().unwrap_or(0.0),
        "timing_final": result.timing_trace.last().copied().unwrap_or(0.0),
        "note": "Post-PLL soft-symbol constellation from the retired Phase 6D \
                 software LSM demod pipeline. 4 clusters expected for a clean LSM \
                 signal (±1, ±j quadrants).",
    }))
}

#[cfg(not(target_os = "linux"))]
async fn get_constellation(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "constellation only available on the target (linux/arm)",
    }))
}

/// `GET /api/modulation` — returns current mode + NID-valid rates
/// for both decoders so the dashboard can show why auto-detect
/// picked what it did.
async fn get_modulation(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let c4fm = state.decoder.read().await;
    let lsm  = state.lsm_decoder.read().await;
    Json(serde_json::json!({
        "mode": state.active_modulation.load(std::sync::atomic::Ordering::Relaxed),
        "label": state.active_modulation_label(),
        "nid_decoded_ok": {
            "c4fm": c4fm.nid_decoded_ok,
            "lsm":  lsm.nid_decoded_ok,
        },
        "tsdu_ok": {
            "c4fm": c4fm.nid_decoded_tsdu,
            "lsm":  lsm.nid_decoded_tsdu,
        },
        "note": "mode: 0=auto, 1=c4fm, 2=lsm. PUT /api/modulation?set=c4fm|lsm|auto to override.",
    }))
}

/// `PUT /api/modulation?set=c4fm|lsm|auto` — manual override. When
/// `auto`, the background auto-detect task picks whichever decoder
/// is producing more BCH-valid NIDs.
async fn put_modulation(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let Some(set) = params.get("set") else {
        return Json(serde_json::json!({
            "ok": false,
            "error": "missing ?set=c4fm|lsm|auto",
        }));
    };
    let code: u8 = match set.as_str() {
        "auto" | "0" => 0,
        "c4fm" | "1" => 1,
        "lsm"  | "2" => 2,
        other => {
            return Json(serde_json::json!({
                "ok": false,
                "error": format!("unknown mode '{other}'; expected c4fm|lsm|auto"),
            }));
        }
    };
    state
        .active_modulation
        .store(code, std::sync::atomic::Ordering::Relaxed);
    Json(serde_json::json!({
        "ok": true,
        "mode": code,
        "label": state.active_modulation_label(),
    }))
}

/// `POST /api/set_time?unix_ms=<i64>`
///
/// Sets the board's wall clock to the given Unix epoch (in ms).
/// Designed for isolated networks — on an RNDIS-over-USB link or
/// any setup without routable internet, the standard NTP-on-boot
/// path fails, and the board sits at 1970-01-01 forever. The
/// dashboard calls this with `Date.now()` every time it loads, so
/// the board ends up with whatever time the browser knows. Not as
/// precise as real NTP (limited to ~HTTP round-trip-jitter) but
/// good enough for event-log ordering + wall-clock display.
///
/// POST (not PUT) because it mutates system state outside /api/.
#[cfg(target_os = "linux")]
async fn post_set_time(
    State(_state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let Some(ms_str) = params.get("unix_ms") else {
        return Json(serde_json::json!({
            "ok": false,
            "error": "missing ?unix_ms=<epoch_ms>",
        }));
    };
    let ms: i64 = match ms_str.parse() {
        Ok(v) => v,
        Err(_) => {
            return Json(serde_json::json!({
                "ok": false,
                "error": format!("bad unix_ms '{ms_str}' (expected integer)"),
            }));
        }
    };
    // Sanity: 2020-01-01 to 2070-01-01 in milliseconds. Guards
    // against a misbehaving browser clock / bogus query.
    if !(1_577_836_800_000..3_155_760_000_000).contains(&ms) {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("unix_ms {ms} out of sane range (2020..2070)"),
        }));
    }
    let tv = libc::timeval {
        tv_sec: (ms / 1000) as libc::time_t,
        tv_usec: ((ms % 1000) * 1000) as libc::suseconds_t,
    };
    let rc = unsafe { libc::settimeofday(&tv, std::ptr::null()) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Json(serde_json::json!({
            "ok": false,
            "error": format!("settimeofday failed: {err}"),
        }));
    }
    Json(serde_json::json!({
        "ok": true,
        "set_unix_ms": ms,
        "note": "Wall clock updated. Use this on boards with no NTP reachability (RNDIS, air-gapped).",
    }))
}

#[cfg(not(target_os = "linux"))]
async fn post_set_time(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "set_time only available on the target (linux/arm)",
    }))
}

/// Hand-maintained catalogue of /api/* endpoints. New routes MUST
/// add an entry here in the same commit that adds them to
/// `router()`; the dashboard's API tab renders from this list.
///
/// Order: routes in alphabetical-by-path order so the table is
/// deterministic. Method is GET unless noted; mixed-method routes
/// (GET+PUT/POST) list both.
struct EndpointDoc {
    method: &'static str,
    path: &'static str,
    params: &'static str,
    description: &'static str,
}

const ENDPOINT_CATALOGUE: &[EndpointDoc] = &[
    EndpointDoc {
        method: "GET",
        path: "/api/aliases",
        params: "",
        description: "Return the talkgroup-alias map (TG number → display name).",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/aliases",
        params: "body=JSON {tg: name, ...}",
        description: "Replace the alias map. Body is a JSON object keyed by TG number.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/audio",
        params: "?format=wav",
        description: "Stream live vocoder PCM as an open-ended WAV (8 kHz 16-bit mono).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/audio_test",
        params: "",
        description: "One-shot ring dump of the vocoder's internal test tone buffer; diagnostic.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/bands",
        params: "",
        description: "List known P25 identifier_update frequency bands (base, spacing, offset, BW).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/bch_t",
        params: "?side=control|traffic",
        description: "Read the runtime BCH(63,16,t) error-correction cap for each decoder.",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/bch_t",
        params: "?side=control|traffic&t=<0..11>",
        description: "Override the BCH-t cap at runtime without rebuilding. Reset with t=reset.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/decoder_compare",
        params: "",
        description: "3-column matrix: PS C4FM vs PS LSM framer vs PL HDL LSM runtime stats.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/decoder_reset",
        params: "?side=control|traffic",
        description: "Reset the framer state of one of the decoders (keeps cumulative counters).",
    },
    EndpointDoc {
        method: "POST",
        path: "/api/decoder_reset",
        params: "?side=control|traffic",
        description: "Same as GET /api/decoder_reset; HTTP-method-correct variant.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/dibit_dump",
        params: "",
        description: "Sample the C4FM dibit ring and return histogram + raw DUID hits for inspection.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/encrypted_tgs",
        params: "",
        description: "Read the sticky encrypted-TG history set (TGs ever seen encrypted).",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/encrypted_tgs",
        params: "body=JSON [tg, tg, ...]",
        description: "Overwrite the encrypted-TG blocklist. Useful for manual curation.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/endpoints",
        params: "",
        description: "This catalogue. Self-describing list of every /api/* route.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/grants",
        params: "",
        description: "Active voice-channel grants (one entry per TG currently on a traffic channel).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/hdl_lsm",
        params: "",
        description: "PL HDL LSM chain runtime snapshot: NID events, NAC histogram, PLL/sync debug taps.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/imbe_dump",
        params: "",
        description: "Dump recent IMBE frame batches for offline vocoder cross-check.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/irq_stats",
        params: "",
        description: "Per-source IRQ counters (dibit / iq / lsm_dibit / traffic DMAs).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/log",
        params: "?since=<seq>&limit=<n>&category=<name>",
        description: "Event log ring. Monotonic seq for incremental tail reads.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_iq_capture",
        params: "",
        description: "One-shot capture of the control chain's post-DDC IQ ring (62.5 kSPS).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_iq_capture_aligned",
        params: "",
        description: "Sync-aligned control IQ capture for offline software-pipeline cross-check.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_lsm_control",
        params: "?lsm_enable=0|1&dma_enable=0|1&dc_block=0|1",
        description: "Runtime read/write of the control-chain lsm_control register bits.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/control_lsm_dibit_dump",
        params: "",
        description: "Histogram of the control-chain LSM demod dibit ring (not C4FM).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_iq_capture",
        params: "",
        description: "Traffic-chain twin of /api/control_iq_capture. Rolling dibit snapshot.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_iq_capture_aligned",
        params: "",
        description: "Traffic-chain twin of /api/control_iq_capture_aligned. Arms next sync hit.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_lsm_control",
        params: "?dc_block=0|1&agc=0|1",
        description: "Runtime read/write of the traffic-chain lsm_control register bits.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic_lsm_dibit_dump",
        params: "",
        description: "Histogram of the traffic-chain LSM demod dibit ring.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/rx_gain",
        params: "?db=<i32>",
        description: "Read or set AD9361 manual RX hardwaregain in dB (range -3..76).",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/rx_gain",
        params: "?db=<i32>",
        description: "PUT twin for /api/rx_gain — same semantics as the GET form.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/grant_map",
        params: "",
        description: "Accumulated grant-frequency map (tg, freq) with counts + last-seen.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/modulation",
        params: "",
        description: "Read current P25 modulation (C4FM/LSM/Auto) + per-decoder NID-valid rates.",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/modulation",
        params: "?set=c4fm|lsm|auto",
        description: "Override or release modulation selection. Auto picks whichever decoder has more valid NIDs.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/monitor",
        params: "",
        description: "Read the TG monitor list (when non-empty, only listed TGs get followed).",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/monitor",
        params: "?add=<tg>&remove=<tg>",
        description: "Add/remove a TG from the monitor list. Empty list = newest-grant-wins.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/nid_capture",
        params: "",
        description: "Batched NID capture for t-sweep analysis by tools/p25_nid_analyze.py.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/recent_tsbks",
        params: "",
        description: "Last ~50 decoded TSBKs with summaries for the activity feed.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/recordings",
        params: "",
        description: "Ring of recent call recordings: id, TG, started_unix_ms, duration_ms, size_bytes.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/recordings/{id}",
        params: "path id, trailing .wav optional",
        description: "Download a recording as WAV (8 kHz 16-bit mono).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/reinit",
        params: "?rx_lo=&control_freq=&sample_rate=&rf_bandwidth=&gain_mode=&gain_db=",
        description: "Live front-end + DDC re-init without reboot. Unspecified fields use boot defaults.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/constellation",
        params: "?chain=control|traffic",
        description: "Post-PLL soft-symbol (I, Q) scatter via the software LSM demod pipeline.",
    },
    EndpointDoc {
        method: "POST",
        path: "/api/set_time",
        params: "?unix_ms=<epoch_ms>",
        description: "Force the wall clock from a browser-pushed value. Fallback for boards without NTP reachability.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/spectrum",
        params: "?chain=control|traffic",
        description: "4096-pt FFT over post-DDC IQ ring, mag_db array fftshifted. Narrowband (~62.5 kHz span).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/stats",
        params: "",
        description: "Decoder stats + AD9361 readback (gain/RSSI/rx_lo/rf_bandwidth/ddc offset/uptime).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/sync_tune",
        params: "?side=control|traffic",
        description: "Read the per-decoder runtime sync threshold.",
    },
    EndpointDoc {
        method: "PUT",
        path: "/api/sync_tune",
        params: "?side=control|traffic&threshold=<0..24>|reset",
        description: "Override the sync-detector Hamming-distance threshold at runtime.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/system",
        params: "",
        description: "System identity: WACN/NAC/RFSS/site, build tag, control channel.",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/traffic",
        params: "?follower=on|off&reset_stats=1&retune_hz=<i64>&demod_enable=0|1",
        description: "Traffic-follower state + manual debug knobs (retune_hz routes through full chain).",
    },
    EndpointDoc {
        method: "GET",
        path: "/api/tsbk_opcodes",
        params: "",
        description: "Histogram of TSBK opcodes observed. Labels match SDRTrunk OSP opcode names.",
    },
    EndpointDoc {
        method: "GET",
        path: "/ws/audio",
        params: "",
        description: "WebSocket binary stream of AudioChunk payloads (used by dashboard player).",
    },
    EndpointDoc {
        method: "GET",
        path: "/ws/events",
        params: "",
        description: "WebSocket text stream of decoder + traffic events as JSON lines.",
    },
];

async fn get_endpoints(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let items: Vec<_> = ENDPOINT_CATALOGUE
        .iter()
        .map(|e| {
            serde_json::json!({
                "method":      e.method,
                "path":        e.path,
                "params":      e.params,
                "description": e.description,
            })
        })
        .collect();
    Json(serde_json::json!({
        "count": items.len(),
        "items": items,
    }))
}

// ── REST Handlers ──────────────────────────────────────────────────────

async fn get_system(State(state): State<Arc<AppState>>) -> Json<SystemInfo> {
    // 2026-04-16 modulation selector: picks LSM (Clay/Duval) or
    // C4FM (FP&L, St Johns) based on AppState.active_modulation,
    // auto-detected by the background task in main.rs that watches
    // nid_decoded_ok delta across both decoders.
    let dec = state.active_control_decoder().read().await;
    let s = &dec.system;
    let system_clock_str = s.last_sync_clock.map(
        |(y, mo, d, h, mn, locked)| {
            format!(
                "{:04}-{:02}-{:02} {:02}:{:02} {}",
                y, mo, d, h, mn,
                if locked { "LOCKED" } else { "UNLOCKED" }
            )
        },
    );
    Json(SystemInfo {
        nac: s.nac.map(|n| format!("{}", n)),
        wacn: s.wacn.map(|w| format!("{:05X}", w)),
        system_id: s.system_id.map(|v| format!("{:03X}", v)),
        rfss_id: s.rfss_id,
        site_id: s.site_id,
        lra: s.lra,
        control_channel: s.control_channel.map(|c| format!("{}", c)),
        secondary_cch_a: s.secondary_cch_a.map(|c| format!("{}", c)),
        secondary_cch_b: s.secondary_cch_b.map(|c| format!("{}", c)),
        sndcp_downlink_channel: s.sndcp_downlink_channel.map(|c| format!("{}", c)),
        sndcp_uplink_channel: s.sndcp_uplink_channel.map(|c| format!("{}", c)),
        system_clock: system_clock_str,
        build: Some(crate::BUILD_TAG.to_string()),
        phase: Some(
            if s.has_tdma_band { "P25 P1+P2" } else { "P25 P1" }.into(),
        ),
    })
}

async fn get_grants(State(state): State<Arc<AppState>>) -> Json<Vec<ChannelGrant>> {
    // Phase 9 retirement (2026-04-15): used to union grants from
    // `lsm_decoder` + `iq_lsm_decoder`. The software pipeline is
    // gone so this is now a single-decoder read. As a side effect,
    // the stale-age bug on the Active Grants panel (iq_lsm_decoder
    // had no expire_grants loop; its grants sat forever) is fixed.
    //
    // Phase 7F.4 (2026-04-14): cross-reference each grant against
    // the persistent `encrypted_tg_history` HashSet. Grants whose
    // current TSBK service options lack the encrypted bit but whose
    // TG has ever been seen encrypted get `in_encrypted_history=true`
    // so the dashboard can badge them even when the latest
    // transmission forgot to set the flag.
    let encrypted_history: std::collections::HashSet<u16> = state
        .imbe_forwarder
        .encrypted_tg_history
        .lock()
        .map(|h| h.clone())
        .unwrap_or_default();

    // 2026-04-16: read from whichever decoder the modulation selector
    // picks (LSM for Clay/Duval, C4FM for FP&L/St Johns).
    let dec = state.active_control_decoder().read().await;
    let mut by_channel: std::collections::HashMap<u16, ChannelGrant> =
        std::collections::HashMap::new();
    for g in dec.grants.values() {
        let in_history = encrypted_history.contains(&g.talkgroup.0);
        let cg = ChannelGrant {
            channel: format!("{}", g.channel),
            talkgroup: g.talkgroup.0,
            talkgroup_alias: dec.aliases.get(&g.talkgroup.0).cloned(),
            source: g.source.map(|s| s.0),
            frequency_mhz: g.frequency_hz.map(|f| f as f64 / 1_000_000.0),
            age_secs: g.timestamp.elapsed().as_secs(),
            encrypted: g.encrypted,
            emergency: g.emergency,
            in_encrypted_history: in_history,
        };
        by_channel.insert(g.channel.0, cg);
    }

    // Second pass: collapse by talkgroup, picking the youngest
    // surviving channel entry per TG. TG 0 is excluded from the
    // dedup (matches the decoder-side wildcard sentinel) so we
    // never collapse multiple unrelated "no-talkgroup" entries.
    let mut by_talkgroup: std::collections::HashMap<u16, ChannelGrant> =
        std::collections::HashMap::new();
    let mut tg0_passthrough: Vec<ChannelGrant> = Vec::new();
    for cg in by_channel.into_values() {
        if cg.talkgroup == 0 {
            tg0_passthrough.push(cg);
            continue;
        }
        match by_talkgroup.get(&cg.talkgroup) {
            Some(existing) if existing.age_secs <= cg.age_secs => {}
            _ => {
                by_talkgroup.insert(cg.talkgroup, cg);
            }
        }
    }
    let mut grants: Vec<ChannelGrant> = by_talkgroup.into_values().collect();
    grants.extend(tg0_passthrough);
    grants.sort_by_key(|g| g.age_secs);
    Json(grants)
}

async fn get_bands(State(state): State<Arc<AppState>>) -> Json<Vec<BandInfo>> {
    // Phase 9 retirement: single-decoder read (was unioning
    // `lsm_decoder` with the retired Phase 6D `iq_lsm_decoder`).
    // 2026-04-16: now respects active_modulation.
    let dec = state.active_control_decoder().read().await;
    let mut bands: Vec<BandInfo> = dec
        .bands
        .values()
        .map(|b| BandInfo {
            identifier: b.identifier,
            base_frequency_mhz: b.base_frequency_hz as f64 / 1_000_000.0,
            channel_spacing_khz: b.channel_spacing_hz as f64 / 1_000.0,
            transmit_offset_mhz: b.transmit_offset_hz as f64 / 1_000_000.0,
            bandwidth_khz: b.bandwidth_hz as f64 / 1_000.0,
        })
        .collect();
    bands.sort_by_key(|b| b.identifier);
    Json(bands)
}

async fn get_stats(State(state): State<Arc<AppState>>) -> Json<DecoderStats> {
    // 2026-04-16: stats read from the active control-chain decoder.
    let decoder = state.active_control_decoder().read().await;

    #[cfg(target_os = "linux")]
    let (dibit_count, overflow, dma_next_address) = {
        let core = state.ip_core.lock().await;
        (
            core.dibit_count() as u32,
            core.demod_overflow(),
            core.dibit_next_address(),
        )
    };
    #[cfg(not(target_os = "linux"))]
    let (dibit_count, overflow, dma_next_address) = (0u32, false, 0u32);

    // AD9361 health: AGC gain (high = AGC searching for weak signal) and
    // RSSI (relative dB scale; for this band, ~100-110 dB is normal P25
    // reception, lower = quieter). Surfacing these via /api/stats so we
    // never have to ssh in and devmem just to find out the radio is alive.
    //
    // Board Info extension: rf_bandwidth, sampling_frequency, gain mode,
    // and the live RX LO are read the same way so the dashboard's
    // "Board Info" panel has one endpoint to poll for everything.
    #[cfg(target_os = "linux")]
    let (
        rx_gain_db,
        rx_rssi_db,
        rx_lo_hz,
        rf_bandwidth_hz,
        sampling_frequency_hz,
        gain_control_mode,
    ) = {
        let g = state.ad9361.get_rx_gain().await.ok();
        let r = state.ad9361.get_rx_rssi().await.ok();
        let lo = state.ad9361.get_rx_lo_frequency().await.ok();
        let bw = state.ad9361.get_rx_rf_bandwidth().await.ok();
        let sr = state.ad9361.get_sampling_frequency().await.ok();
        let gm = state
            .ad9361
            .get_rx_gain_mode()
            .await
            .ok()
            .map(|m| m.to_string());
        (g, r, lo, bw, sr, gm)
    };
    #[cfg(not(target_os = "linux"))]
    let (
        rx_gain_db,
        rx_rssi_db,
        rx_lo_hz,
        rf_bandwidth_hz,
        sampling_frequency_hz,
        gain_control_mode,
    ): (
        Option<f64>,
        Option<f64>,
        Option<u64>,
        Option<u32>,
        Option<u32>,
        Option<String>,
    ) = (None, None, None, None, None, None);

    // DDC geometry: the control-side DDC NCO sits at a fixed offset
    // from the LO (plus a small crystal-ppm correction). Report that
    // offset so the operator can see "which DDC frequency is the
    // control channel" without re-deriving it from /api/reinit.
    let ddc_control_offset_hz: Option<i64> = rx_lo_hz.map(|lo| {
        let nco_lo_shift_hz = -state.boot_lo_ppm * 1e-6 * lo as f64;
        (state.boot_control_freq as f64 - lo as f64 + nco_lo_shift_hz) as i64
    });
    // Decimation chain is a compile-time constant of the HDL build.
    // Phase 10-prep redesign: /4 /4 /8 Parks-McClellan split.
    // 8 MSPS ADC / 128 = 62.5 kSPS into the demod.
    let ddc_decimation = Some("/4 /4 /8 = /128".to_string());
    let ddc_output_rate_hz = sampling_frequency_hz.map(|sr| sr / 128);

    // Wall clock: Linux clock value. Pre-NTP this will read 1970-...;
    // post-NTP it's real. We format it here so the browser doesn't
    // have to parse a raw u64 seconds-since-epoch.
    let wall_clock = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| {
                let secs = d.as_secs();
                // Minimal ISO-ish formatter without pulling chrono in.
                let (year, month, day, h, m, s) = ts_to_ymd_hms(secs);
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    year, month, day, h, m, s
                )
            })
    };
    let uptime_secs = Some(state.boot_instant.elapsed().as_secs());

    let audio_ws_lag_total = Some(
        state
            .audio_ws_lag_total
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let audio_ws_clients = Some(state.audio_tx.receiver_count());

    Json(DecoderStats {
        recent_messages: decoder.recent_messages.len(),
        active_grants: decoder.grants.len(),
        bands_known: decoder.bands.len(),
        system_acquired: decoder.system.wacn.is_some(),
        dibit_count,
        overflow,
        dma_next_address,
        rx_gain_db,
        rx_rssi_db,
        rx_lo_hz,
        rf_bandwidth_hz,
        sampling_frequency_hz,
        gain_control_mode,
        ddc_control_offset_hz,
        ddc_decimation,
        ddc_output_rate_hz,
        wall_clock,
        uptime_secs,
        audio_ws_lag_total,
        audio_ws_clients,
    })
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

/// Returns recent dibits as a hex string + diagnostic counters.
///
/// Each pair of hex chars = 8 dibits. Useful for sanity-checking
/// the demod output from a browser without devmem on the target.
async fn get_dibit_dump(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let decoder = state.decoder.read().await;
    Json(dibit_dump_json(&decoder, "C4FM HDL chain (c4fm_dibit_dma)"))
}

/// Phase 6F.2: LSM-side counterpart of `/api/dibit_dump`. Same diagnostic
/// shape but reads from `lsm_decoder` (the software decoder fed by
/// `lsm_dibit_dma`). Lets us compare the LSM dibit stream's histogram /
/// sync correlator / raw_DUID distribution against the C4FM stream side
/// by side without having to grep the on-target log.
async fn get_control_lsm_dibit_dump(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let decoder = state.lsm_decoder.read().await;
    Json(dibit_dump_json(&decoder, "PL HDL LSM chain (lsm_dibit_dma)"))
}

/// Phase 10-prep: traffic-side counterpart of `/api/control_lsm_dibit_dump`.
/// Reads from `traffic_lsm_decoder` so we can diagnose the traffic
/// framer's slicer / sync correlator / raw_DUID distribution without
/// waiting for a grant. Same response shape as the control-side
/// endpoint so dashboard / tooling can treat them symmetrically.
async fn get_traffic_lsm_dibit_dump(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let decoder = state.traffic_lsm_decoder.read().await;
    Json(dibit_dump_json(
        &decoder, "PL HDL traffic LSM chain (traffic_lsm_dibit_dma)"))
}

/// Phase 6F.2h diagnostic capture endpoint.
///
/// Returns the LSM decoder's `recent_dibits` rolling buffer (up to 2048
/// raw on-air dibits) as a base64-encoded byte array, one byte per
/// dibit (only the low 2 bits used). Also returns a hex-string view
/// for human readability and the cumulative dibit counter at capture
/// time so a follow-up call can detect overlaps.
///
/// Arming the next-sync alignment capture is a separate endpoint
/// (`/api/control_iq_capture_aligned`); this one just returns
/// whatever's currently in the rolling buffer with no waiting.
async fn get_control_iq_capture(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let decoder = state.lsm_decoder.read().await;
    let dibits: Vec<u8> = decoder.recent_dibits.iter().copied().collect();
    let hex: String = dibits.iter().map(|d| format!("{:1X}", d & 0x3)).collect();
    Json(serde_json::json!({
        "captured":     dibits.len(),
        "total_dibits": decoder.total_dibits(),
        "dibits_hex":   hex,
        "note": "One hex digit per dibit, oldest first. Each digit is the \
                 low 2 bits (00..03). 4800 sym/s -> 2048 dibits ~= 426 ms.",
    }))
}

/// Phase 10-prep: traffic-side counterpart of `/api/control_iq_capture`.
/// Rolling recent-dibits buffer from the traffic LSM decoder.
async fn get_traffic_iq_capture(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let decoder = state.traffic_lsm_decoder.read().await;
    let dibits: Vec<u8> = decoder.recent_dibits.iter().copied().collect();
    let hex: String = dibits.iter().map(|d| format!("{:1X}", d & 0x3)).collect();
    Json(serde_json::json!({
        "chain":        "traffic",
        "captured":     dibits.len(),
        "total_dibits": decoder.total_dibits(),
        "dibits_hex":   hex,
        "note": "Same shape as /api/control_iq_capture, but reads from the \
                 traffic-chain rolling dibit buffer so post-retune slicer \
                 behaviour can be inspected without a call being active.",
    }))
}

/// Phase 6F.2h diagnostic capture endpoint -- aligned snapshot.
///
/// Arms the LSM decoder to capture the next sync hit and returns the
/// full pipeline trace through that one frame:
///   - 24 sync dibits
///   - 33 raw NID dibits (status dibit at index 11 not yet skipped)
///   - 64-bit nid_bits word fed to BCH (status dibit removed, packed
///     MSB-first)
///   - BCH-corrected NAC + DUID + raw DUID
///   - 122 raw TSDU body dibits
///   - 98 trellis data dibits after status + null removal
///   - 12 trellis-decoded TSBK bytes
///   - CRC validation result (Plain | Xored | None)
///
/// This is the data we use to bisect between "deinterleaver bug" and
/// "trellis bug" if the dashboard counters say PS LSM is still failing
/// CRC after 6F.2g lands.
///
/// **Behaviour:** the endpoint is one-shot per call. It arms the
/// capture flag, then waits up to 2 seconds for the next sync hit. If
/// no sync hits in that window it returns `{"status": "timeout"}`.
/// Otherwise it returns the snapshot and clears the armed state.
async fn get_control_iq_capture_aligned(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::time::{Duration, Instant};

    // Arm the capture: clear any previous snapshot, set the armed flag.
    {
        let mut dec = state.lsm_decoder.write().await;
        dec.aligned_capture = None;
        dec.aligned_capture_armed = true;
    }

    // Poll for up to 10 seconds. The LSM dibit reader IRQ fires only
    // about every 3.5 seconds (one buffer per IRQ), and the reader
    // holds the decoder write() lock for the duration of one buffer
    // (~8000 dibits). So the API may have to wait up to 2 IRQ cycles
    // before it can read a populated capture. 10 s gives us at least
    // 3 cycles of headroom -- if no sync hits in that long, the chain
    // is genuinely stalled.
    let deadline = Instant::now() + Duration::from_millis(10000);
    loop {
        {
            let dec = state.lsm_decoder.read().await;
            if let Some(snap) = dec.aligned_capture.as_ref() {
                return Json(snap.to_json());
            }
        }
        if Instant::now() >= deadline {
            // Disarm so we don't capture later than the user expects.
            let mut dec = state.lsm_decoder.write().await;
            dec.aligned_capture_armed = false;
            return Json(serde_json::json!({
                "status": "timeout",
                "note": "No sync hit observed within 2 seconds. Either the \
                         LSM dibit stream is stalled (check IRQ counters) \
                         or the sync correlator is missing every frame \
                         (check best Hamming distance on the LSM dibit dump).",
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Phase 10-prep: traffic-side counterpart of
/// `/api/control_iq_capture_aligned`. Same arm + wait protocol but
/// against the traffic LSM decoder. Useful for debug-capturing a
/// post-retune TDU or LDU frame to see where the slicer / framer
/// is landing before the grant follower cancels the retune.
async fn get_traffic_iq_capture_aligned(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::time::{Duration, Instant};

    {
        let mut dec = state.traffic_lsm_decoder.write().await;
        dec.aligned_capture = None;
        dec.aligned_capture_armed = true;
    }

    // Traffic framer only sees sync hits while a real call is active
    // (the noise-gate changes landing in the Phase 10 bake will make
    // this even more true). Give up to 15 seconds of headroom in
    // case the user is arming this just before a grant arrives.
    let deadline = Instant::now() + Duration::from_millis(15000);
    loop {
        {
            let dec = state.traffic_lsm_decoder.read().await;
            if let Some(snap) = dec.aligned_capture.as_ref() {
                return Json(snap.to_json());
            }
        }
        if Instant::now() >= deadline {
            let mut dec = state.traffic_lsm_decoder.write().await;
            dec.aligned_capture_armed = false;
            return Json(serde_json::json!({
                "status": "timeout",
                "chain":  "traffic",
                "note": "No sync hit on the traffic chain within 15 s. \
                         Likely no active call during the capture window -- \
                         arm again while a grant is in progress for a \
                         populated snapshot.",
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Phase 6F.7: read the current runtime sync threshold + a quick
/// histogram-based "tuning hint" so an operator can decide what to
/// set next without rebuilding the dashboard.
///
/// **Dual-mode endpoint:** if called with `?threshold=N`, this also
/// updates the runtime threshold (mirroring the PUT handler) so it
/// works from a browser bar or plain `curl` without `-X PUT`. The
/// response always contains the *current* (post-update) value.
async fn get_sync_tune(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;

    // GET-with-query-param shortcut: if `threshold=N` is present and
    // valid, update the runtime threshold before returning the
    // histogram.
    let mut updated_from = None;
    if let Some(v) = params.get("threshold") {
        if let Ok(n) = v.parse::<u32>() {
            if n <= 24 {
                let prev = RUNTIME_SYNC_THRESHOLD.swap(n, Ordering::Relaxed);
                updated_from = Some(prev);
            }
        }
    }

    let dec = state.lsm_decoder.read().await;
    let cur = RUNTIME_SYNC_THRESHOLD.load(Ordering::Relaxed);
    let hist = dec.sync_distance_hist;

    // Cumulative counts at each prospective threshold (0..=24).
    let mut cumulative = [0u64; 25];
    let mut running = 0u64;
    for (i, &v) in hist.iter().enumerate() {
        running += v;
        cumulative[i] = running;
    }
    let total: u64 = hist.iter().sum();

    Json(serde_json::json!({
        "current_threshold":   cur,
        "default_threshold":   SYNC_THRESHOLD,
        "updated_from":        updated_from,
        "total_observations":  total,
        "cumulative_at_threshold": cumulative,
        "histogram": hist,
        "note": "GET /api/sync_tune?threshold=N updates the runtime sync \
                 threshold in-place (no PUT needed). Range 0..=24. Use \
                 the histogram + cumulative arrays to pick a threshold \
                 that captures the real-sync cluster (clear bump above \
                 the binomial random tail) without flooding the \
                 pipeline with noise. After tuning, GET \
                 /api/decoder_reset clears counters so you can measure \
                 the new threshold against a clean baseline.",
    }))
}

/// Phase 6F.7: clear the per-run decoder counters and histograms
/// without restarting the binary. Lets us measure a new
/// `/api/sync_tune?threshold=N` value against a clean baseline
/// instead of waiting hours for the cumulative counters to wash out.
///
/// What this clears: NID counters, TSDU counters, TSBK CRC counters,
/// per-opcode histograms, per-block-position counters, sync hits,
/// sync near misses, sync distance histogram, recent_messages ring,
/// raw_duid_hist, dibit_hist.
///
/// What this PRESERVES: system identity (NAC/WACN/RFSS/...),
/// frequency band table, active grants, talkgroup aliases. Those are
/// long-lived radio state that shouldn't be wiped just because we
/// want a clean measurement window.
async fn get_decoder_reset(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    // Phase 9 retirement: only `lsm_decoder` left to reset (the
    // Phase 6D `iq_lsm_decoder` is gone).
    let mut dec = state.lsm_decoder.write().await;
    dec.reset_diagnostics();
    Json(serde_json::json!({
        "ok": true,
        "note": "lsm_decoder counters + histograms cleared. System \
                 identity, bands, grants, and aliases preserved.",
    }))
}

async fn post_decoder_reset(state: State<Arc<AppState>>) -> Json<serde_json::Value> {
    get_decoder_reset(state).await
}

/// Phase 6G.2: read-back of the `lsm_control` register, plus an
/// optional GET-with-query-param shortcut for toggling
/// `lsm_dc_block_enable` without ssh + devmem.
///
/// Without query params, returns the current state of all three
/// `lsm_control` bits + a hint about which bit positions they map
/// to. The dashboard can poll this once a second to surface the
/// "is the DC blocker actually on?" question that previously
/// required scraping the startup log.
///
/// With `?dc_block=0` or `?dc_block=1`, ALSO writes the bit before
/// reading back. This is the runtime A/B knob the doc 030 PL port
/// roadmap (and Phase 6G.1 verification plan in doc 031) wanted
/// but had to do via `devmem` previously. Range-checked: only
/// `0` or `1` are accepted, everything else is ignored. The two
/// other lsm_control bits (`lsm_enable`, `lsm_dibit_dma_enable`)
/// are NOT exposed for write here -- those are master enables that
/// shouldn't be flipped at runtime, and there's no debugging story
/// that needs them.
///
/// Returns the same shape whether or not the write happened, so a
/// curl-based A/B test loop can just toggle and re-read in one
/// request:
///
/// ```text
/// curl http://192.168.2.1:8080/api/control_lsm_control?dc_block=0
/// curl http://192.168.2.1:8080/api/control_lsm_control?dc_block=1
/// ```
async fn get_control_lsm_control(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let mut updated_from: Option<bool> = None;

    #[cfg(target_os = "linux")]
    {
        // Take the ip_core lock once, do both the optional write and
        // the readback under it so nothing can race in between.
        let core = state.ip_core.lock().await;

        if let Some(v) = params.get("dc_block") {
            let new_val = match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            };
            if let Some(new_bit) = new_val {
                let (_, _, prev) = core.lsm_control_readback();
                core.set_lsm_dc_block_enable(new_bit);
                updated_from = Some(prev);
            }
        }

        let (lsm_en, lsm_dma_en, lsm_dc_block) = core.lsm_control_readback();
        Json(serde_json::json!({
            "lsm_enable":            lsm_en,
            "lsm_dibit_dma_enable":  lsm_dma_en,
            "lsm_dc_block_enable":   lsm_dc_block,
            "updated_from":          updated_from,
            "register_address":      "0x7C4600A0",
            "bit_layout": {
                "lsm_enable":            "[0]",
                "lsm_dibit_dma_enable":  "[1]",
                "lsm_dc_block_enable":   "[2]"
            },
            "note": "GET /api/control_lsm_control?dc_block=0 disables the LSM \
                     front-end DC blocker; ?dc_block=1 enables it. The \
                     other two bits are not writable from this endpoint \
                     -- toggle them via devmem if you really need to. \
                     See doc/changes/031 + 032 for the rationale.",
        }))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (state, params, &mut updated_from);
        Json(serde_json::json!({
            "ok": false,
            "error": "lsm_control read/write requires hardware (target_os=linux)",
        }))
    }
}

/// Phase 10-prep: traffic-side counterpart of
/// `/api/control_lsm_control`. Reads/writes the `traffic_lsm_control`
/// HDL register:
///   bit 0: traffic_lsm_enable
///   bit 1: traffic_lsm_dibit_dma_enable
///   bit 2: traffic_lsm_dc_block_enable
///   bit 3: traffic_lsm_agc_enable  (Phase 10-prep)
///
/// Currently only `dc_block` and `agc` are writable from this
/// endpoint; the enable + dma_enable bits are managed by
/// `retune_traffic_chain` and shouldn't be flipped out-of-band.
async fn get_traffic_lsm_control(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let mut updated_dc: Option<bool> = None;
    let mut updated_agc: Option<bool> = None;

    #[cfg(target_os = "linux")]
    {
        let core = state.ip_core.lock().await;

        if let Some(v) = params.get("dc_block") {
            if let Some(new_bit) = match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            } {
                let (_, _, prev) = core.traffic_lsm_control_readback();
                core.set_traffic_lsm_dc_block_enable(new_bit);
                updated_dc = Some(prev);
            }
        }

        if let Some(v) = params.get("agc") {
            if let Some(new_bit) = match v.as_str() {
                "1" | "true" => Some(true),
                "0" | "false" => Some(false),
                _ => None,
            } {
                core.set_traffic_lsm_agc_enable(new_bit);
                updated_agc = Some(!new_bit);
            }
        }

        let (en, dma_en, dc_block) = core.traffic_lsm_control_readback();
        Json(serde_json::json!({
            "chain":                        "traffic",
            "traffic_lsm_enable":           en,
            "traffic_lsm_dibit_dma_enable": dma_en,
            "traffic_lsm_dc_block_enable":  dc_block,
            "updated_dc_block_from":        updated_dc,
            "updated_agc_from":             updated_agc,
            "note": "GET /api/traffic_lsm_control?dc_block=0|1 toggles \
                     the traffic-chain DC blocker; ?agc=0|1 toggles the \
                     per-symbol AGC. The enable + dibit_dma_enable bits \
                     are managed by retune_traffic_chain and are \
                     read-only here.",
        }))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (state, params, &mut updated_dc, &mut updated_agc);
        Json(serde_json::json!({
            "ok": false,
            "error": "traffic_lsm_control requires hardware (target_os=linux)",
        }))
    }
}

/// Phase 10-prep: GET /api/rx_gain -- AD9361 RX gain knob.
///
/// Read-only without params. With `?db=<int>` sets
/// `in_voltage0_hardwaregain` via IIO and returns the new reading.
/// Range [-3, 76] dB in 1 dB steps.
async fn get_rx_gain(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    #[cfg(target_os = "linux")]
    {
        let mut updated_from: Option<i64> = None;
        let mut error: Option<String> = None;

        if let Some(v) = params.get("db") {
            match v.parse::<i64>() {
                Ok(db) if (-3..=76).contains(&db) => {
                    let prev = state.ad9361.get_rx_gain().await.ok()
                        .map(|f: f64| f as i64);
                    match state.ad9361.set_rx_gain(db as f64).await {
                        Ok(()) => {
                            updated_from = prev;
                            state.event_log.push(
                                crate::event_log::LogCategory::System,
                                format!("rx_gain set to {db} dB"),
                                serde_json::json!({
                                    "db": db, "previous": prev,
                                }),
                            );
                        }
                        Err(e) => error = Some(format!("set_rx_gain: {e}")),
                    }
                }
                Ok(_) => error = Some(
                    "db out of range [-3, 76]".to_string()),
                Err(e) => error = Some(format!("parse db: {e}")),
            }
        }

        let gain: Option<f64> = state.ad9361.get_rx_gain().await.ok();
        let mode = state.ad9361.get_rx_gain_mode().await.ok()
            .map(|m| m.to_string());
        let rssi: Option<f64> = state.ad9361.get_rx_rssi().await.ok();

        Json(serde_json::json!({
            "gain_db":       gain,
            "mode":          mode,
            "rssi_db":       rssi,
            "updated_from":  updated_from,
            "error":         error,
            "range_db":      [-3, 76],
            "note": "GET /api/rx_gain?db=N sets manual hardwaregain in dB \
                     (step 1 dB, range [-3, 76]). Returns the live readback.",
        }))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (state, params);
        Json(serde_json::json!({
            "ok": false,
            "error": "rx_gain requires hardware (target_os=linux)",
        }))
    }
}

/// PUT twin for /api/rx_gain so method-correct clients can call it.
async fn put_rx_gain(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    get_rx_gain(State(state), axum::extract::Query(params)).await
}

/// Phase 10-prep: GET /api/grant_map -- accumulated grant-frequency
/// map. Every grant on the control channel is tallied here by
/// (tg, frequency) — count, last-seen timestamp, encryption count.
///
/// Used by (a) the scanner-mode UI as a TG picker, (b) a future
/// auto-center-LO endpoint to pick an RX LO that keeps the most
/// active traffic channels in-band.
async fn get_grant_map(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let mgr = state.traffic_manager.lock().await;
    let rows: Vec<serde_json::Value> = mgr.grant_map.iter()
        .map(|(key, entry)| serde_json::json!({
            "tg":                key.0,
            "frequency_hz":      key.1,
            "count":             entry.count,
            "encrypted_count":   entry.encrypted_count,
            "first_seen_unix_ms": entry.first_seen_unix_ms,
            "last_seen_unix_ms":  entry.last_seen_unix_ms,
        }))
        .collect();
    let total_entries = rows.len();
    let total_grants: u64 = mgr.grant_map.values().map(|e| e.count).sum();

    // Frequency-only roll-up for LO-centering discussion.
    let mut freq_counts: std::collections::HashMap<u64, u64> =
        std::collections::HashMap::new();
    let mut freq_tgs: std::collections::HashMap<u64,
        std::collections::HashSet<u16>> =
        std::collections::HashMap::new();
    for (key, entry) in mgr.grant_map.iter() {
        *freq_counts.entry(key.1).or_insert(0) += entry.count;
        freq_tgs.entry(key.1).or_default().insert(key.0);
    }
    let mut frequencies: Vec<serde_json::Value> = freq_counts.into_iter()
        .map(|(hz, n)| serde_json::json!({
            "frequency_hz": hz,
            "count":        n,
            "distinct_tgs": freq_tgs.get(&hz).map(|s| s.len()).unwrap_or(0),
        }))
        .collect();
    frequencies.sort_by(|a, b| {
        b.get("count").and_then(|v| v.as_u64()).unwrap_or(0)
            .cmp(&a.get("count").and_then(|v| v.as_u64()).unwrap_or(0))
    });

    Json(serde_json::json!({
        "entries":       rows,
        "total_entries": total_entries,
        "total_grants":  total_grants,
        "frequencies":   frequencies,
        "note": "Accumulated since p25-httpd start. Each (tg, frequency) \
                 pair is one row with count + first/last seen. \
                 Frequencies roll-up groups by frequency only, sorted by \
                 activity — use the top entries to decide where to center \
                 the AD9361 LO so the most active slots stay in-band.",
    }))
}

/// Phase 7A.1: GET /api/traffic -- traffic-channel grant follower
/// state + dibit DMA counters, with optional manual control via
/// query parameters.
///
/// **Read-side** (no params): returns a snapshot of the TrafficManager
/// state machine, the TrafficStats counters, and the traffic_dma IRQ
/// count.
///
/// **Write-side** (query params, applied in this order before reading
/// the snapshot below):
///
/// 1. `?reset_stats=1` -- zero out the TrafficStats counters
///    (wakeups, total_*, dibit_hist). Useful for clean A/B
///    comparisons after a config change.
/// 2. `?follower=on|off` -- pause/resume the 50 ms grant-follower
///    polling task in main.rs. When `off`, manual retunes won't be
///    immediately overridden by the next snapshot. Default state is
///    `on`; the override does NOT persist across p25-httpd restarts.
/// 3. `?retune_hz=<i64>` -- manually retune the traffic DDC to the
///    supplied NCO offset in Hz, signed, relative to the AD9361 RX
///    LO. Routes through the full `retune_traffic_chain` sequence
///    (disable -> NCO write -> 2 ms FIR flush -> re-enable -> reset
///    pulse -> demod enable), same as the grant follower. Bypasses
///    the follower's PPM correction, so the offset you supply is
///    what the register sees -- useful for measuring PPM error
///    directly. Resets the traffic framer before the retune.
/// 4. `?demod_enable=0|1` -- manually flip the
///    `traffic_demod_control.demod_enable` register bit. Rarely
///    needed now that #3 leaves demod_enable=1, but kept for
///    explicit debug control (e.g. forcing demod_enable=0 to
///    snapshot the chain in a quiet state).
///
/// All four params can be combined in one call:
/// `GET /api/traffic?follower=off&reset_stats=1&retune_hz=2862500&demod_enable=1`
/// will pause the follower, zero the counters, retune to RX LO + 2.8625
/// MHz, and turn on the demod -- in that order, so the histogram
/// counts only what arrives after the retune.
///
/// At Phase 7A.1 the traffic chain is C4FM-only and Clay County is
/// LSM, so the dibit *content* is expected garbage on real LSM voice
/// channels. The histogram is included as a sanity check: a dead
/// chain produces all-zero dibits, a live chain produces a roughly
/// even spread across all four dibit values. Phase 7A.2 will add an
/// LSM parallel chain on the traffic side and the histogram will
/// become decode-quality data.
async fn get_traffic(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;

    // Track which write actions actually fired so the JSON response
    // can echo them back -- gives the caller a confirmation that the
    // params were parsed and applied (vs. silently ignored due to a
    // typo).
    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // ── 1. reset_stats ──
    if params.get("reset_stats").map(String::as_str) == Some("1") {
        let mut s = state.traffic_stats.lock().await;
        *s = crate::TrafficStats::default();
        applied.push("reset_stats=1".into());
    }

    // ── 2. follower on/off ──
    if let Some(v) = params.get("follower") {
        match v.as_str() {
            "on" | "1" | "true" => {
                state
                    .traffic_follower_enabled
                    .store(true, Ordering::Relaxed);
                applied.push("follower=on".into());
            }
            "off" | "0" | "false" => {
                state
                    .traffic_follower_enabled
                    .store(false, Ordering::Relaxed);
                applied.push("follower=off".into());
            }
            other => {
                errors.push(format!(
                    "follower={other}: expected on|off|1|0|true|false"
                ));
            }
        }
    }

    // ── 3. retune_hz (manual NCO write) ──
    //    Linux-only because it touches the FPGA registers via the
    //    ip_core lock. The non-Linux build path simply records an
    //    error so host-side cargo test of the routing still works.
    if let Some(v) = params.get("retune_hz") {
        match v.parse::<i64>() {
            Ok(offset_hz) => {
                #[cfg(target_os = "linux")]
                {
                    // 2026-04-16 fix: route manual retune through the
                    // full retune_traffic_chain sequence (disable ->
                    // NCO write -> 2 ms FIR flush -> re-enable ->
                    // reset pulse -> demod enable) so the debug path
                    // matches the grant follower's production path.
                    // Also reset the traffic framer so stale dibits
                    // from the previous NCO don't feed a half-
                    // processed state on the new frequency.
                    {
                        let mut dec = state.traffic_lsm_decoder
                            .write().await;
                        dec.reset_framer_state();
                    }
                    let core = state.ip_core.lock().await;
                    let sample_rate_hz = 8_000_000.0_f64;
                    match core.retune_traffic_chain(
                        offset_hz as f64,
                        sample_rate_hz,
                    ) {
                        Ok(()) => {
                            applied.push(format!(
                                "retune_hz={offset_hz} (full chain)"
                            ));
                            // Mirror manager-side bookkeeping so
                            // /api/traffic shows the new offset
                            // immediately even though the follower
                            // didn't drive it.
                            let mut mgr =
                                state.traffic_manager.lock().await;
                            mgr.last_offset_hz = offset_hz;
                            let nco_frac =
                                offset_hz as f64 / sample_rate_hz;
                            mgr.nco_word = (nco_frac
                                * (1u64 << 28) as f64)
                                as i32
                                as u32
                                & 0x0FFF_FFFF;
                        }
                        Err(e) => {
                            errors.push(format!(
                                "retune_hz={offset_hz} rejected: {e}"
                            ));
                        }
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = offset_hz;
                    errors.push(
                        "retune_hz requires hardware (target_os=linux)"
                            .into(),
                    );
                }
            }
            Err(_) => {
                errors.push(format!(
                    "retune_hz={v}: expected signed integer Hz offset"
                ));
            }
        }
    }

    // ── 4. demod_enable ──
    if let Some(v) = params.get("demod_enable") {
        let parsed = match v.as_str() {
            "1" | "on" | "true" => Some(true),
            "0" | "off" | "false" => Some(false),
            _ => None,
        };
        match parsed {
            Some(bit) => {
                #[cfg(target_os = "linux")]
                {
                    let core = state.ip_core.lock().await;
                    core.set_traffic_demod_enable(bit);
                    applied.push(format!("demod_enable={bit}"));
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = bit;
                    errors.push(
                        "demod_enable requires hardware (target_os=linux)"
                            .into(),
                    );
                }
            }
            None => {
                errors.push(format!(
                    "demod_enable={v}: expected 0|1|on|off|true|false"
                ));
            }
        }
    }

    // ── Snapshot read (always, even after a write) ──
    let (
        state_label,
        current_channel,
        current_talkgroup,
        current_frequency_hz,
        nco_word,
        last_offset_hz,
        grants_seen,
        retunes,
        grants_rejected_encrypted,
        last_retune_at_secs_ago,
        last_duid,
        last_nac,
        hdus_seen,
        ldus_seen,
        tdus_seen,
        post_tdu_hold_remaining_ms,
    ) = {
        let mgr = state.traffic_manager.lock().await;
        let label = mgr.state_label();
        let ch = mgr.current_channel().map(|c| c.0);
        let tg = mgr.current_talkgroup().map(|t| t.0);
        let freq = mgr.current_frequency();
        let nco = mgr.nco_word;
        let offset = mgr.last_offset_hz;
        let seen = mgr.grants_seen;
        let retunes = mgr.retunes;
        let rejected_enc = mgr.grants_rejected_encrypted;
        let age = mgr
            .last_retune_at
            .map(|t| t.elapsed().as_secs_f64());
        let duid = mgr.last_duid;
        let nac = mgr.last_nac;
        let hdus = mgr.hdus_seen;
        let ldus = mgr.ldus_seen;
        let tdus = mgr.tdus_seen;
        let hold = mgr.post_tdu_hold_remaining_ms();
        (label, ch, tg, freq, nco, offset, seen, retunes, rejected_enc,
         age, duid, nac, hdus, ldus, tdus, hold)
    };

    // Phase 7A.2: read the live traffic_lsm chain health from the
    // FPGA registers. Mirrors the control-side `/api/hdl_lsm` snapshot
    // but on the new traffic_lsm bank.
    #[cfg(target_os = "linux")]
    let traffic_lsm_chain_json = {
        let core = state.ip_core.lock().await;
        let s = core.traffic_lsm_status();
        let drop_count = core.traffic_lsm_drop_count();
        let last_buffer = core.traffic_lsm_dibit_last_buffer();
        let next_addr = core.traffic_lsm_dibit_next_address();
        let (pll_dbg, sample_point_dbg) = core.traffic_lsm_debug();
        let (en, dma_en, dc_block) = core.traffic_lsm_control_readback();
        serde_json::json!({
            "enabled":            en,
            "dibit_dma_enabled":  dma_en,
            "dc_block_enabled":   dc_block,
            "bch_busy":           s.bch_busy,
            "in_nid_window":      s.in_nid_window,
            "nid_event":          s.nid_event,
            "nid_valid":          s.nid_valid,
            "n_errors":           s.n_errors,
            "sync_distance":      s.sync_distance,
            "dibit_overflow":     s.dibit_overflow,
            "drop_count":         drop_count,
            "dibit_last_buffer":  last_buffer,
            "dibit_next_addr":    format!("0x{:08X}", next_addr),
            "pll_dbg":            pll_dbg,
            "sample_point_dbg":   sample_point_dbg,
        })
    };
    #[cfg(not(target_os = "linux"))]
    let traffic_lsm_chain_json = serde_json::json!(null);

    let stats_json = {
        let s = state.traffic_stats.lock().await;
        let total: u64 = s.dibit_hist.iter().sum();
        let pct = |v: u64| -> f64 {
            if total == 0 {
                0.0
            } else {
                100.0 * v as f64 / total as f64
            }
        };
        serde_json::json!({
            "wakeups":        s.wakeups,
            "total_buffers":  s.total_buffers,
            "total_bytes":    s.total_bytes,
            "total_dibits":   s.total_dibits,
            "dibit_hist":     s.dibit_hist,
            "dibit_hist_pct": [
                pct(s.dibit_hist[0]), pct(s.dibit_hist[1]),
                pct(s.dibit_hist[2]), pct(s.dibit_hist[3])
            ],
            "started_secs_ago": s.started_at.map(|t| t.elapsed().as_secs_f64()),
            "last_secs_ago":    s.last_at.map(|t| t.elapsed().as_secs_f64()),
        })
    };

    let irq_json = {
        let s = state.irq_stats.lock().await;
        serde_json::json!({
            "traffic_dma_total":       s.traffic,
            "traffic_lsm_dibit_total": s.traffic_lsm_dibit,
        })
    };

    // Phase 7C: IMBE counter snapshot from the traffic LSM voice
    // Phase 7D: read IMBE extraction + vocoder stats from the
    // ImbeForwarder's atomics. Updated synchronously by the voice
    // handler (extraction counters) and by the vocoder task (PCM
    // produced, errors, encrypted skips).
    let imbe_json = {
        use std::sync::atomic::Ordering;
        let c = &state.imbe_forwarder;
        let last_imbe_at_millis = c.last_imbe_at_millis.load(Ordering::Relaxed);
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last_imbe_secs_ago = if last_imbe_at_millis == 0 {
            None
        } else {
            Some((now_millis.saturating_sub(last_imbe_at_millis)) as f64 / 1000.0)
        };
        serde_json::json!({
            "hdu_count":              c.hdu_count.load(Ordering::Relaxed),
            "ldu1_count":             c.ldu1_count.load(Ordering::Relaxed),
            "ldu2_count":             c.ldu2_count.load(Ordering::Relaxed),
            "tdu_count":              c.tdu_count.load(Ordering::Relaxed),
            "tdu_lc_count":           c.tdu_lc_count.load(Ordering::Relaxed),
            "imbe_frames_extracted":  c.imbe_frames_extracted.load(Ordering::Relaxed),
            "imbe_frames_dropped":    c.imbe_frames_dropped.load(Ordering::Relaxed),
            "imbe_frames_dropped_idle": c.imbe_frames_dropped_idle.load(Ordering::Relaxed),
            "last_imbe_secs_ago":     last_imbe_secs_ago,
            "vocoder_pcm_produced":   c.vocoder_pcm_produced.load(Ordering::Relaxed),
            "vocoder_errors":         c.vocoder_errors.load(Ordering::Relaxed),
            "vocoder_frames_encrypted": c.vocoder_frames_encrypted.load(Ordering::Relaxed),
        })
    };

    // Phase 7C: also surface the traffic_lsm_decoder's own
    // sync/decode counters so the dashboard can see whether the
    // decoder's framer is finding sync hits on the traffic dibit
    // stream (the most important bring-up signal -- if sync_hits
    // == 0 we know the dibit ring isn't carrying anything the
    // decoder recognises as P25).
    let traffic_lsm_decoder_json = {
        let d = state.traffic_lsm_decoder.read().await;
        serde_json::json!({
            "sync_hits":          d.sync_hits(),
            "sync_near_misses":   d.sync_near_misses(),
            "best_sync_distance": if d.best_sync_distance() == u32::MAX { 99 } else { d.best_sync_distance() },
            "recent_msg_count":   d.recent_messages.len(),
            "ldu1":               d.ldu1_count,
            "ldu2":               d.ldu2_count,
            "hdu":                d.hdu_count,
            "tdu":                d.tdu_count,
            "tdu_lc":             d.tdu_lc_count,
        })
    };

    // Phase 7C: pull the encryption flag from the currently-locked
    // grant, if any. The grant follower's TrafficManager holds the
    // Phase 7D: the encryption flag shown on the dashboard comes from
    // the ImbeForwarder's `call_encrypted` atomic, which is the same
    // flag the vocoder task reads to decide whether to decode or skip.
    // This is set by the grant follower task whenever it observes a
    // grant for the locked TG, and cleared on Idle transition.
    // Single source of truth: what the vocoder sees = what the
    // dashboard shows.
    let current_call_encrypted = {
        let mgr = state.traffic_manager.lock().await;
        if mgr.current_talkgroup().is_some() {
            Some(state.imbe_forwarder.call_encrypted.load(
                std::sync::atomic::Ordering::Relaxed,
            ))
        } else {
            None
        }
    };

    let follower_on =
        state.traffic_follower_enabled.load(Ordering::Relaxed);

    Json(serde_json::json!({
        "state":                     state_label,
        "follower_enabled":          follower_on,
        "current_channel":           current_channel,
        "current_talkgroup":         current_talkgroup,
        "current_frequency_hz":      current_frequency_hz,
        "nco_word":                  nco_word,
        "nco_word_hex":              format!("0x{:08X}", nco_word),
        "last_offset_hz":            last_offset_hz,
        "grants_seen":               grants_seen,
        "retunes":                   retunes,
        "grants_rejected_encrypted": grants_rejected_encrypted,
        "last_retune_secs_ago":      last_retune_at_secs_ago,
        // Phase 7A.2: NID event counters and post-TDU hold
        "last_duid":                 last_duid,
        "last_duid_hex":             last_duid.map(|d| format!("0x{:X}", d)),
        "last_duid_label":           last_duid.map(|d| match d {
            0x0 => "HDU",
            0x3 => "TDU",
            0x5 => "LDU1",
            0x7 => "TSDU",
            0xA => "LDU2",
            0xC => "PDU",
            0xF => "TDU_LC",
            _   => "?",
        }),
        "last_nac":                  last_nac,
        "last_nac_hex":              last_nac.map(|n| format!("0x{:03X}", n)),
        "hdus_seen":                 hdus_seen,
        "ldus_seen":                 ldus_seen,
        "tdus_seen":                 tdus_seen,
        "post_tdu_hold_remaining_ms": post_tdu_hold_remaining_ms,
        "stats":                     stats_json,
        "irq":                       irq_json,
        "traffic_lsm_chain":         traffic_lsm_chain_json,
        "applied":                   applied,
        "errors":                    errors,
        "phase":                     "7C",
        "modulation":                "C4FM + LSM (parallel chains, LSM is the active one for HDU/TDU/LDU dispatch + IMBE extraction)",
        // Phase 7C: encryption flag from the control channel grant
        // for the currently-locked TG (None if no call active or
        // no grant in store). Phase 7D will read this to skip the
        // vocoder for encrypted calls.
        "current_call_encrypted":    current_call_encrypted,
        // Phase 7C: IMBE frame counter snapshot from the
        // traffic_lsm_decoder's voice handler.
        "imbe":                      imbe_json,
        // Phase 7C: traffic_lsm_decoder framer state (sync hits,
        // recent msgs, per-DUID counters from the decoder itself).
        // Distinct from `imbe` above which counts via the voice
        // handler atomic counters: these are the decoder-internal
        // counters and should track 1:1 with the imbe ones.
        "traffic_lsm_decoder":       traffic_lsm_decoder_json,
        "controls": {
            "reset_stats":   "?reset_stats=1            -- zero TrafficStats",
            "follower":      "?follower=on|off          -- pause/resume 50 ms poll",
            "retune_hz":     "?retune_hz=<i64>          -- manual NCO offset (Hz, signed)",
            "demod_enable":  "?demod_enable=0|1         -- manual demod_enable bit (C4FM chain only -- LSM chain has its own enable in HDL)"
        },
        "note": "Phase 7C: traffic-side LSM dibit reader feeds a \
                 ControlChannelDecoder that runs the same Hunting -> \
                 ReadingNid -> ReadingDataUnit state machine as the \
                 control side. On LDU1/LDU2 dispatch the decoder \
                 strips status dibits, applies the 9 IMBE bit \
                 positions from SDRTrunk LDUMessage.java, and emits \
                 raw 144-bit IMBE frames via the VoiceHandler trait. \
                 Phase 7D will plug a vocoder into the same \
                 callback chain.",
    }))
}

/// Phase 6F.7: PUT /api/sync_tune?threshold=N -- update the runtime
/// sync threshold without rebuilding. Validates 0 <= N <= 24.
async fn put_sync_tune(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;
    // Phase 7F.5 (2026-04-14): add optional ?side= param so the
    // traffic-side decoder can be tuned independently of the global
    // threshold. ?side=traffic|control sets the per-decoder
    // override; omitting ?side= updates the global.
    let side = params.get("side").cloned();
    let new = params
        .get("threshold")
        .and_then(|v| {
            if v == "reset" || v == "null" {
                Some(None)
            } else {
                v.parse::<u32>().ok().map(Some)
            }
        });

    match (side.as_deref(), new) {
        (Some("traffic"), Some(Some(n))) if n <= 24 => {
            state.traffic_lsm_decoder.write().await
                .set_sync_threshold_override(Some(n));
            state.event_log.push(
                crate::event_log::LogCategory::System,
                format!("sync threshold: traffic-side -> {}", n),
                serde_json::json!({"side":"traffic","value":n}),
            );
            Json(serde_json::json!({
                "ok": true, "side": "traffic", "current": n,
            }))
        }
        (Some("traffic"), Some(None)) => {
            state.traffic_lsm_decoder.write().await
                .set_sync_threshold_override(None);
            Json(serde_json::json!({
                "ok": true, "side": "traffic", "current": "reset",
            }))
        }
        (Some("control"), Some(Some(n))) if n <= 24 => {
            state.lsm_decoder.write().await
                .set_sync_threshold_override(Some(n));
            state.event_log.push(
                crate::event_log::LogCategory::System,
                format!("sync threshold: control-side -> {}", n),
                serde_json::json!({"side":"control","value":n}),
            );
            Json(serde_json::json!({
                "ok": true, "side": "control", "current": n,
            }))
        }
        (Some("control"), Some(None)) => {
            state.lsm_decoder.write().await
                .set_sync_threshold_override(None);
            Json(serde_json::json!({
                "ok": true, "side": "control", "current": "reset",
            }))
        }
        (None, Some(Some(n))) if n <= 24 => {
            let prev = RUNTIME_SYNC_THRESHOLD.swap(n, Ordering::Relaxed);
            Json(serde_json::json!({
                "ok":            true,
                "previous":      prev,
                "current":       n,
                "default":       SYNC_THRESHOLD,
                "note":          "Global threshold updated. Counters keep \
                                  accumulating; use /api/sync_tune to verify \
                                  the new histogram shape after a few seconds \
                                  of new data. Use ?side=traffic|control to \
                                  set a per-decoder override instead.",
            }))
        }
        _ => Json(serde_json::json!({
            "ok":     false,
            "error":  "missing or invalid `threshold` (0..=24 or 'reset'); \
                       optional ?side=traffic|control for per-decoder override",
            "current_global": RUNTIME_SYNC_THRESHOLD.load(Ordering::Relaxed),
        })),
    }
}

/// Phase 6F.4: per-opcode histogram of CRC-OK and CRC-FAIL TSBK
/// blocks decoded by the LSM software decoder. Lets the dashboard
/// see the on-air opcode distribution and pinpoint missing parsers.
async fn get_tsbk_opcodes(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec = state.lsm_decoder.read().await;

    // Map opcode index → SDRTrunk-style label so the dashboard
    // doesn't have to mirror the table. Covers all opcodes that
    // appear in `Opcode.java` for the OSP direction (control-channel
    // outbound). Lowercase here means we don't recognise it as a P25
    // opcode at all (probably trellis-decode garbage).
    fn label(op: u8) -> &'static str {
        match op {
            0x00 => "GRP_V_CH_GRANT",
            0x02 => "GRP_V_CH_GRANT_UPDT",
            0x03 => "GRP_V_CH_GRANT_UPDT_EXP",
            0x04 => "UU_V_CH_GRANT",
            0x05 => "UU_ANS_REQ",
            0x06 => "UU_V_CH_GRANT_UPDT",
            0x08 => "TELE_INT_V_CH_GRANT",
            0x09 => "TELE_INT_V_CH_GRANT_UPDT",
            0x0A => "TELE_INT_ANS_REQ",
            0x14 => "SNDCP_DCH_GRANT",
            0x15 => "SNDCP_DCH_PAG_RQ",
            0x16 => "SNDCP_DCH_ANN_EX",
            0x18 => "STS_UPDT",
            0x1C => "MSG_UPDT",
            0x1F => "CALL_ALERT",
            0x20 => "ACK_RESPONSE_FNE",
            0x21 => "QUEUED_RESP",
            0x22 => "EXT_FNCT_CMD",
            0x24 => "DENY_RESPONSE",
            0x27 => "GRP_AFFIL_RESP",
            0x28 => "SCCB",
            0x29 => "RFSS_STS_BCST_EXP",
            0x2A => "NET_STS_BCST_EXP",
            0x2B => "ADJ_STS_BCST_EXP",
            0x2C => "IDEN_UP_VUHF_EXP",
            0x2D => "DENY_RESPONSE_EXP",
            0x2F => "DE_REGIST_ACK",
            0x30 => "TDMA_SYNC_BCST",
            0x31 => "AUTH_DMD",
            0x32 => "AUTH_FNE_RESULT",
            0x33 => "IDEN_UPDATE_TDMA",
            0x34 => "IDEN_UPDATE_VUHF",
            0x36 => "TIME_DATE",
            0x37 => "ROAM_ADDR_CMD",
            0x38 => "SYS_SRV_BCST",
            0x39 => "SEC_CCH_BROADCST",
            0x3A => "RFSS_STATUS_BCST",
            0x3B => "NET_STATUS_BCAST",
            0x3C => "ADJ_STS_BCAST",
            0x3D => "IDEN_UPDATE",
            0x3E => "PROT_PARAM_BCST",
            0x3F => "PROT_PARAM_UPDT",
            _ => "(unknown)",
        }
    }

    let mut entries = Vec::with_capacity(64);
    let mut total_ok = 0u64;
    let mut total_fail = 0u64;
    for op in 0u8..64 {
        let ok = dec.tsbk_opcode_hist_ok[op as usize];
        let fail = dec.tsbk_opcode_hist_fail[op as usize];
        total_ok += ok;
        total_fail += fail;
        if ok > 0 || fail > 0 {
            let parsed = matches!(
                op,
                // 6F.4 + 6F.5: voice grants, IDEN_UPDATE variants,
                // RFSS / NET / ADJ status broadcasts.
                0x00 | 0x02 | 0x33 | 0x34 | 0x3A | 0x3B | 0x3C | 0x3D
                // 6F.11: 5 new parsers added in this phase.
                | 0x05 | 0x09 | 0x16 | 0x30 | 0x39
            );
            entries.push(serde_json::json!({
                "opcode": format!("0x{:02X}", op),
                "label": label(op),
                "ok": ok,
                "fail": fail,
                "parsed": parsed,
            }));
        }
    }
    // Sort by ok-count descending so the most common live opcodes
    // float to the top of the list.
    entries.sort_by(|a, b| {
        let a_ok = a["ok"].as_u64().unwrap_or(0);
        let b_ok = b["ok"].as_u64().unwrap_or(0);
        b_ok.cmp(&a_ok)
    });

    // Per-block-position rates (TSBK1 / TSBK2 / TSBK3 attempts and
    // CRC successes). If TSBK2 / TSBK3 success rates are massively
    // worse than TSBK1, the multi-block continuation alignment is
    // wrong somewhere upstream.
    let attempts_pos = dec.tsbk_block_attempts_by_pos;
    let crc_ok_pos = dec.tsbk_crc_ok_by_pos;
    let pos_pct = |a: u64, ok: u64| -> f64 {
        if a == 0 { 0.0 } else { 100.0 * ok as f64 / a as f64 }
    };

    Json(serde_json::json!({
        "tsdu_attempts": dec.tsdu_attempts,
        "tsbk_block_attempts_total": dec.tsbk_block_attempts,
        "blocks_per_tsdu": if dec.tsdu_attempts == 0 { 0.0 }
            else { dec.tsbk_block_attempts as f64 / dec.tsdu_attempts as f64 },
        "crc_ok_total": total_ok,
        "crc_fail_total": total_fail,
        "crc_ok_pct": if (total_ok + total_fail) == 0 { 0.0 }
            else { 100.0 * total_ok as f64 / (total_ok + total_fail) as f64 },
        "by_position": {
            "tsbk1": {
                "attempts": attempts_pos[0],
                "crc_ok": crc_ok_pos[0],
                "crc_ok_pct": pos_pct(attempts_pos[0], crc_ok_pos[0]),
            },
            "tsbk2": {
                "attempts": attempts_pos[1],
                "crc_ok": crc_ok_pos[1],
                "crc_ok_pct": pos_pct(attempts_pos[1], crc_ok_pos[1]),
            },
            "tsbk3": {
                "attempts": attempts_pos[2],
                "crc_ok": crc_ok_pos[2],
                "crc_ok_pct": pos_pct(attempts_pos[2], crc_ok_pos[2]),
            },
        },
        "mfid_breakdown": {
            "standard_0x00": dec.tsbk_mfid_hist_ok[0],
            "motorola_0x90": dec.tsbk_mfid_hist_ok[1],
            "harris_0xA4":   dec.tsbk_mfid_hist_ok[2],
            "other":         dec.tsbk_mfid_hist_ok[3],
        },
        "opcodes": entries,
    }))
}

/// Phase 6F.4: dump the most recent TSBK messages with their
/// originating block index (TSBK1/2/3), so the dashboard can show a
/// live activity feed in the same format as SDRTrunk's
/// decoded_messages.log.
async fn get_recent_tsbks(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec = state.lsm_decoder.read().await;
    let now = std::time::Instant::now();

    let summarize = |msg: &crate::p25::tsbk::TsbkMessage| -> String {
        use crate::p25::tsbk::TsbkMessage::*;
        match msg {
            NetworkStatus { wacn, system_id, channel } => format!(
                "NET_STATUS_BCAST WACN:{:05X} SYS:{:03X} CH:{}",
                wacn, system_id, channel
            ),
            RfssStatus { lra, rfss_id, site_id, channel } => format!(
                "RFSS_STATUS_BCST LRA:{} RFSS:{} SITE:{} CH:{}",
                lra, rfss_id, site_id, channel
            ),
            AdjacentStatus { lra, rfss_id, site_id, channel, system_id } => format!(
                "ADJ_STS_BCAST LRA:{} SYS:{:03X} RFSS:{} SITE:{} CH:{}",
                lra, system_id, rfss_id, site_id, channel
            ),
            IdentifierUpdate {
                identifier,
                bw,
                transmit_offset,
                channel_spacing,
                base_frequency,
            } => format!(
                "IDEN_UPDATE ID:{} OFFSET:{} SPACING:{} BASE:{} BW:{}",
                identifier, transmit_offset, channel_spacing, base_frequency, bw
            ),
            GroupVoiceChannelGrant { channel, talkgroup, source, service_options } => format!(
                "GRP_V_CH_GRANT CH:{} TG:{} SRC:{}{}",
                channel, talkgroup, source,
                if crate::p25::tsbk::service_options::is_encrypted(*service_options) {
                    " [ENC]"
                } else {
                    ""
                }
            ),
            GroupVoiceChannelGrantUpdate {
                channel_a, talkgroup_a, channel_b, talkgroup_b,
            } => format!(
                "GRP_V_CH_GRANT_UPDT CH_A:{} TG_A:{} CH_B:{} TG_B:{}",
                channel_a, talkgroup_a, channel_b, talkgroup_b
            ),
            GroupVoiceChannelGrantUpdateExplicit {
                transmit_channel, receive_channel, talkgroup, service_options,
            } => format!(
                "GRP_V_CH_GRANT_UPDT_EXP TX:{} RX:{} TG:{}{}",
                transmit_channel, receive_channel, talkgroup,
                if crate::p25::tsbk::service_options::is_encrypted(*service_options) {
                    " [ENC]"
                } else {
                    ""
                }
            ),
            // Phase 6F.11 new opcodes
            SecondaryControlChannelBroadcast {
                rfss_id, site_id, channel_a, channel_b,
            } => format!(
                "SEC_CCH_BROADCST RFSS:{} SITE:{} A:{} B:{}",
                rfss_id, site_id, channel_a, channel_b
            ),
            SndcpDataChannelAnnouncementExplicit {
                downlink_channel, uplink_channel, autonomous_access,
                requested_access, ..
            } => format!(
                "SNDCP_DCH_ANN_EX DL:{} UL:{} {}{}",
                downlink_channel, uplink_channel,
                if *autonomous_access { "AUTO " } else { "" },
                if *requested_access { "REQ" } else { "" },
            ),
            TdmaSyncBroadcast {
                year, month, day, hours, minutes, time_locked, ..
            } => format!(
                "TDMA_SYNC_BCST {:04}-{:02}-{:02} {:02}:{:02} {}",
                year, month, day, hours, minutes,
                if *time_locked { "LOCKED" } else { "UNLOCKED" }
            ),
            TelephoneInterconnectVoiceChannelGrantUpdate {
                channel, call_timer_secs, unit_id,
            } => format!(
                "TEL_INT_VCH_GRNT_UPDT UNIT:{} CH:{} timer:{}s",
                unit_id, channel, call_timer_secs
            ),
            UnitToUnitAnswerRequest { target, source } => format!(
                "UU_ANS_REQ TGT:{} SRC:{}", target, source
            ),
        }
    };

    // Iterate newest-first.
    let entries: Vec<serde_json::Value> = dec
        .recent_messages
        .iter()
        .rev()
        .take(50)
        .map(|(t, block_idx, msg)| {
            let block_label = match block_idx {
                0 => "TSBK1",
                1 => "TSBK2",
                2 => "TSBK3",
                _ => "TSBK?",
            };
            serde_json::json!({
                "age_secs": now.duration_since(*t).as_secs_f64(),
                "block": block_label,
                "summary": summarize(msg),
            })
        })
        .collect();

    Json(serde_json::json!({
        "count": entries.len(),
        "messages": entries,
    }))
}

fn dibit_dump_json(
    decoder: &ControlChannelDecoder,
    source_label: &str,
) -> serde_json::Value {
    let dibits: Vec<u8> = decoder.recent_dibits.iter().copied().collect();

    // Pack 4 dibits per byte (LSB first), MSB-first byte ordering
    let mut packed = Vec::with_capacity(dibits.len().div_ceil(4));
    for chunk in dibits.chunks(4) {
        let mut b = 0u8;
        for (i, d) in chunk.iter().enumerate() {
            b |= (d & 0x03) << (i * 2);
        }
        packed.push(b);
    }
    let hex: String = packed.iter().map(|b| format!("{:02X}", b)).collect();

    // Histogram + sync stats
    let hist = decoder.dibit_histogram();
    let total = decoder.total_dibits();
    let pct = |v: u64| -> f64 {
        if total == 0 { 0.0 } else { 100.0 * v as f64 / total as f64 }
    };

    // Inner-vs-outer ratio is the canonical health indicator for the
    // symbol-rate slicer: random P25 data should give ~50/50, and a
    // residual DC bias on sym_diff_re skews it. Today (2026-04-09) we're
    // running at ~70/30 inner/outer because of post-DDC DC pedestal,
    // which is why SYNC_THRESHOLD is currently 10 instead of 4.
    let inner = hist[0] + hist[2]; // values 0 (+1) and 2 (-1)
    let outer = hist[1] + hist[3]; // values 1 (+3) and 3 (-3)

    // Raw on-air DUID histogram. With the BCH(64,16) NID FEC currently
    // stubbed (see fec::GolayDecoder::decode_nid), this tells us how
    // often each 4-bit DUID value lands in the NID field after sync.
    // A healthy control channel + working FEC would be ~100% in bucket 7
    // (TSDU). Today, with no FEC, this is a near-uniform spray due to
    // the ~12 bit errors per NID induced by the slicer DC bias.
    let raw_duid_hist = decoder.raw_duid_histogram();
    let raw_duid_total: u64 = raw_duid_hist.iter().sum();
    let raw_duid_pct = |v: u64| -> f64 {
        if raw_duid_total == 0 { 0.0 } else { 100.0 * v as f64 / raw_duid_total as f64 }
    };

    serde_json::json!({
        "source": source_label,
        "total_dibits": total,
        "captured": dibits.len(),
        "histogram": {
            "0":     hist[0],
            "1":     hist[1],
            "2":     hist[2],
            "3":     hist[3],
            "0_pct": pct(hist[0]),
            "1_pct": pct(hist[1]),
            "2_pct": pct(hist[2]),
            "3_pct": pct(hist[3]),
            "inner_pct": pct(inner),
            "outer_pct": pct(outer),
        },
        "sync": {
            "hits":          decoder.sync_hits(),
            "near_misses":   decoder.sync_near_misses(),
            "best_distance": decoder.best_sync_distance(),
            "threshold":     RUNTIME_SYNC_THRESHOLD.load(std::sync::atomic::Ordering::Relaxed),
            "threshold_default": SYNC_THRESHOLD,
            // Phase 6F.6 distance histogram. Bucket i = count of dibit
            // shifts where the sync_register matched at exactly Hamming
            // distance i. Bucket 24 collects everything ≥ 24. The cluster
            // shape tells us whether the slicer is the bottleneck or
            // whether widening the threshold further would help.
            "distance_hist": decoder.sync_distance_hist,
        },
        "raw_duid": {
            "total": raw_duid_total,
            "counts": raw_duid_hist,
            "pct_7_tsdu": raw_duid_pct(raw_duid_hist[7]),
            "pct_5_ldu1": raw_duid_pct(raw_duid_hist[5]),
            "pct_0_hdu":  raw_duid_pct(raw_duid_hist[0]),
            "pct_a_ldu2": raw_duid_pct(raw_duid_hist[0xA]),
            "note": "Healthy control channel + BCH FEC = ~100% in bucket 7 (TSDU). \
                     Anything else means the NID has uncorrected bit errors.",
        },
        "pipeline": {
            "nid_attempts":          decoder.nid_attempts,
            "nid_decode_failures":   decoder.nid_decode_failures,
            "nid_invalid_duid":      decoder.nid_invalid_duid,
            "nid_decoded_ok":        decoder.nid_decoded_ok,
            "nid_decoded_tsdu":      decoder.nid_decoded_tsdu,
            "tsdu_attempts":         decoder.tsdu_attempts,
            "tsbk_block_attempts":   decoder.tsbk_block_attempts,
            "tsbk_trellis_failures": decoder.tsbk_trellis_failures,
            "tsbk_crc_failures":     decoder.tsbk_crc_failures,
            "tsbk_crc_ok":           decoder.tsbk_crc_ok,
            "tsbk_unknown_opcode":   decoder.tsbk_unknown_opcode,
        },
        "dibits_hex": hex,
    })
}

/// Phase 6D: snapshot of the LSM pipeline runtime stats.
///
// Phase 9 retirement: `get_lsm()` (Phase 6D software pipeline stats
// for the dashboard "LSM Pipeline" card) was removed here along with
// the `LsmStats` struct it read. The PL-side equivalent is
// `/api/hdl_lsm` (below), which taps the HDL register bank directly
// via the `HdlLsmRuntime` heartbeat task. That's the single source of
// truth for "is the LSM chain alive / how many valid NIDs / what
// NACs" now.

/// Phase 6F.2: PL HDL LSM chain runtime snapshot.
///
/// Reads the shared `HdlLsmRuntime` populated by the heartbeat task.
/// Includes: live register snapshot, cumulative NID counts, NAC
/// histogram, and the last 32 NID events from the ring buffer.
async fn get_hdl_lsm(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use std::time::Instant;
    let rt = state.hdl_lsm.lock().await;
    let now = Instant::now();
    let uptime_secs = rt
        .started_at
        .map(|t| now.saturating_duration_since(t).as_secs())
        .unwrap_or(0);
    let last_tick_ms_ago = rt
        .last_tick_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);
    let last_nid_ms_ago = rt
        .last_nid_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);

    let total_nac_hits: u64 = rt.nac_hist.values().sum();
    let top_nacs: Vec<serde_json::Value> = rt
        .top_nacs(10)
        .into_iter()
        .map(|(nac, count)| {
            let pct = if total_nac_hits == 0 {
                0.0
            } else {
                100.0 * (count as f64) / (total_nac_hits as f64)
            };
            serde_json::json!({
                "nac":   format!("0x{:03X}", nac),
                "count": count,
                "pct":   pct,
            })
        })
        .collect();

    Json(serde_json::json!({
        "running":              rt.started_at.is_some(),
        "uptime_secs":          uptime_secs,
        "last_tick_ms_ago":     last_tick_ms_ago,
        "last_nid_ms_ago":      last_nid_ms_ago,

        "live": {
            "pll_dbg":               rt.pll_dbg,
            "sp_dbg":                rt.sp_dbg,
            "sync_distance":         rt.sync_distance,
            "bch_busy":              rt.bch_busy,
            "in_nid_window":         rt.in_nid_window,
            "dibit_overflow_latch":  rt.dibit_overflow_latched,
            "iq_overflow_latch":     rt.iq_overflow_latched,
            "last_nac":              format!("0x{:03X}", rt.last_nac),
            "last_duid":             rt.last_duid,
            "last_drop_count":       rt.last_drop_count,
            "last_nid_valid":        rt.last_nid_valid,
            "last_nid_n_errors":     rt.last_nid_n_errors,
        },

        "cumulative": {
            "total_nid_events":      rt.total_nid_events,
            "valid_nid_events":      rt.valid_nid_events,
            "valid_pct":             if rt.total_nid_events == 0 {
                0.0
            } else {
                100.0 * (rt.valid_nid_events as f64) / (rt.total_nid_events as f64)
            },
            "dibit_overflow_ticks":  rt.dibit_overflow_ticks,
            "iq_overflow_ticks":     rt.iq_overflow_ticks,
        },

        "last_window": {
            "pll_min":               rt.hb_pll_min,
            "pll_max":               rt.hb_pll_max,
            "sp_min":                rt.hb_sp_min,
            "sp_max":                rt.hb_sp_max,
            "sync_dist_best":        rt.hb_sync_dist_best,
            "bch_busy_ticks":        rt.hb_bch_busy_ticks,
            "in_window_ticks":       rt.hb_in_window_ticks,
            "nid_event_ticks":       rt.hb_nid_event_ticks,
            "dibit_overflow_ticks":  rt.hb_dibit_overflow_ticks,
            "iq_overflow_ticks":     rt.hb_iq_overflow_ticks,
            "iq_kbps":               rt.hb_iq_kbps,
            "iq_buf_rolls":          rt.hb_iq_buf_rolls,
            "valid_count":           rt.hb_window_valid_count,
            "event_count":           rt.hb_window_event_count,
        },

        "top_nacs":  top_nacs,
        "nid_ring":  rt.nid_ring.clone(),
    }))
}

/// Phase 6F.2: per-source IRQ counters from the InterruptHandler task.
async fn get_irq_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use std::time::Instant;
    let s = state.irq_stats.lock().await;
    let now = Instant::now();
    let uptime_secs = s
        .started_at
        .map(|t| now.saturating_duration_since(t).as_secs())
        .unwrap_or(0);
    let last_at_ms_ago = s
        .last_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);
    let rate = |n: u64| -> f64 {
        if uptime_secs == 0 { 0.0 } else { (n as f64) / (uptime_secs as f64) }
    };
    Json(serde_json::json!({
        "running":            s.started_at.is_some(),
        "uptime_secs":        uptime_secs,
        "last_at_ms_ago":     last_at_ms_ago,
        "total":              s.total,
        "dibit":              s.dibit,
        "traffic":            s.traffic,
        "iq":                 s.iq,
        "lsm_dibit":          s.lsm_dibit,
        "traffic_lsm_dibit":  s.traffic_lsm_dibit,
        "traffic_iq":         s.traffic_iq,
        "rate_per_sec": {
            "total":             rate(s.total),
            "dibit":             rate(s.dibit),
            "traffic":           rate(s.traffic),
            "iq":                rate(s.iq),
            "lsm_dibit":         rate(s.lsm_dibit),
            "traffic_lsm_dibit": rate(s.traffic_lsm_dibit),
            "traffic_iq":        rate(s.traffic_iq),
        },
    }))
}

/// Phase 9: side-by-side comparison matrix of the surviving decoder
/// sources. Phase 6D software-LSM columns (`ps_iq_lsm` + `ps_phase6d`)
/// were retired along with the iq_lsm_decoder and LsmStats; the
/// dashboard decoder matrix is now PS-C4FM (dormant, kept for
/// future C4FM sites), PS-LSM framer (software framer on HDL LSM
/// dibits — the current production control-channel path), and
/// PL-HDL (the FPGA LSM chain's own runtime stats tapped directly
/// from the register bank).
async fn get_decoder_compare(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec_c4fm = state.decoder.read().await;
    let dec_lsm = state.lsm_decoder.read().await;
    let hdl_rt = state.hdl_lsm.lock().await;

    fn fmt_nac(n: Option<crate::p25::types::Nac>) -> serde_json::Value {
        match n {
            Some(v) => serde_json::Value::String(format!("{}", v)),
            None => serde_json::Value::Null,
        }
    }
    fn fmt_nac_u16(n: u16) -> String { format!("0x{:03X}", n) }

    let hdl_winner_nac = hdl_rt
        .top_nacs(1)
        .first()
        .map(|(n, _)| fmt_nac_u16(*n))
        .unwrap_or_else(|| "--".to_string());

    Json(serde_json::json!({
        "ps_c4fm": {
            "label":           "PS C4FM (software, HDL C4FM dibit-fed — DORMANT on LSM sites)",
            "system_nac":      fmt_nac(dec_c4fm.system.nac),
            "messages":        dec_c4fm.recent_messages.len(),
            "active_grants":   dec_c4fm.grants.len(),
            "bands_known":     dec_c4fm.bands.len(),
            "sync_hits":       dec_c4fm.sync_hits(),
            "sync_near":       dec_c4fm.sync_near_misses(),
            "sync_best_dist":  if dec_c4fm.best_sync_distance() == u32::MAX {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(dec_c4fm.best_sync_distance())
            },
            "total_dibits":    dec_c4fm.total_dibits(),
            "nid_attempts":          dec_c4fm.nid_attempts,
            "nid_decode_failures":   dec_c4fm.nid_decode_failures,
            "nid_invalid_duid":      dec_c4fm.nid_invalid_duid,
            "nid_decoded_ok":        dec_c4fm.nid_decoded_ok,
            "nid_decoded_tsdu":      dec_c4fm.nid_decoded_tsdu,
            "tsdu_attempts":         dec_c4fm.tsdu_attempts,
            "tsbk_block_attempts":   dec_c4fm.tsbk_block_attempts,
            "tsbk_trellis_failures": dec_c4fm.tsbk_trellis_failures,
            "tsbk_crc_failures":     dec_c4fm.tsbk_crc_failures,
            "tsbk_crc_ok":           dec_c4fm.tsbk_crc_ok,
            "tsbk_crc_ok_plain":     dec_c4fm.tsbk_crc_ok_plain,
            "tsbk_crc_ok_xored":     dec_c4fm.tsbk_crc_ok_xored,
            "tsbk_unknown_opcode":   dec_c4fm.tsbk_unknown_opcode,
        },
        "ps_lsm": {
            "label":           "PS LSM framer (software framer on HDL LSM dibits — production)",
            "system_nac":      fmt_nac(dec_lsm.system.nac),
            "messages":        dec_lsm.recent_messages.len(),
            "active_grants":   dec_lsm.grants.len(),
            "bands_known":     dec_lsm.bands.len(),
            "sync_hits":       dec_lsm.sync_hits(),
            "sync_near":       dec_lsm.sync_near_misses(),
            "sync_best_dist":  if dec_lsm.best_sync_distance() == u32::MAX {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(dec_lsm.best_sync_distance())
            },
            "total_dibits":    dec_lsm.total_dibits(),
            "nid_attempts":          dec_lsm.nid_attempts,
            "nid_decode_failures":   dec_lsm.nid_decode_failures,
            "nid_invalid_duid":      dec_lsm.nid_invalid_duid,
            "nid_decoded_ok":        dec_lsm.nid_decoded_ok,
            "nid_decoded_tsdu":      dec_lsm.nid_decoded_tsdu,
            "tsdu_attempts":         dec_lsm.tsdu_attempts,
            "tsbk_block_attempts":   dec_lsm.tsbk_block_attempts,
            "tsbk_trellis_failures": dec_lsm.tsbk_trellis_failures,
            "tsbk_crc_failures":     dec_lsm.tsbk_crc_failures,
            "tsbk_crc_ok":           dec_lsm.tsbk_crc_ok,
            "tsbk_crc_ok_plain":     dec_lsm.tsbk_crc_ok_plain,
            "tsbk_crc_ok_xored":     dec_lsm.tsbk_crc_ok_xored,
            "tsbk_unknown_opcode":   dec_lsm.tsbk_unknown_opcode,
        },
        "pl_hdl": {
            "label":           "PL HDL LSM chain (FPGA gateware)",
            "winner_nac":      hdl_winner_nac,
            "total_nids":      hdl_rt.total_nid_events,
            "valid_nids":      hdl_rt.valid_nid_events,
            "valid_pct":       if hdl_rt.total_nid_events == 0 {
                0.0
            } else {
                100.0 * (hdl_rt.valid_nid_events as f64)
                    / (hdl_rt.total_nid_events as f64)
            },
            "drop_count":      hdl_rt.last_drop_count,
            "pll_dbg":         hdl_rt.pll_dbg,
            "sp_dbg":          hdl_rt.sp_dbg,
            "sync_distance":   hdl_rt.sync_distance,
            "dibit_overflow_ticks": hdl_rt.dibit_overflow_ticks,
            "iq_overflow_ticks":    hdl_rt.iq_overflow_ticks,
        },
    }))
}

async fn get_aliases(State(state): State<Arc<AppState>>) -> Json<AliasMap> {
    let decoder = state.decoder.read().await;
    Json(decoder.aliases.clone())
}

async fn put_aliases(
    State(state): State<Arc<AppState>>,
    Json(aliases): Json<AliasMap>,
) -> impl IntoResponse {
    let mut decoder = state.decoder.write().await;
    decoder.aliases = aliases;
    axum::http::StatusCode::OK
}

// ── WebSocket ──────────────────────────────────────────────────────────

async fn ws_events(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.event_tx.subscribe();
    while let Ok(msg) = rx.recv().await {
        if socket
            .send(axum::extract::ws::Message::Text(msg.into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

// ── Phase 7D diagnostic: raw IMBE frame dump ──────────────────────────

/// GET /api/imbe_dump -- return the last 128 raw IMBE frames from the
/// ring buffer. Each entry has talkgroup, encrypted flag, and the raw
/// 18 bytes (144 bits) in hex. Use for offline vocoder testing.
async fn get_imbe_dump(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let ring = state.imbe_forwarder.imbe_ring.lock().unwrap();
    let frames: Vec<serde_json::Value> = ring
        .iter()
        .map(|(tg, enc, bits)| {
            serde_json::json!({
                "talkgroup": tg,
                "encrypted": enc,
                "hex": bits.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "count": frames.len(),
        "frames": frames,
    }))
}

/// GET /api/audio_test -- decode the IMBE ring buffer on-device and
/// return a WAV file. Filters to clear (non-encrypted) frames only.
/// Use to verify vocoder output without VLC streaming.
///
///   curl -o test.wav http://192.168.2.1:8080/api/audio_test
async fn get_audio_test(
    State(state): State<Arc<AppState>>,
) -> impl axum::response::IntoResponse {
    let ring = state.imbe_forwarder.imbe_ring.lock().unwrap().clone();
    let clear: Vec<_> = ring.iter().filter(|(_, enc, _)| !enc).collect();

    let mut decoder = crate::vocoder::JmbeDecoder::new();
    let mut all_pcm: Vec<i16> = Vec::new();

    for (_tg, _enc, bits) in &clear {
        let frame = crate::p25::voice_frame::ImbeFrameRaw { bits: *bits };
        let pcm = decoder.decode_frame(&frame);
        all_pcm.extend_from_slice(&pcm);
    }

    // Build WAV
    let header = crate::audio::wav_header_8k_16bit_mono();
    let data_size = (all_pcm.len() * 2) as u32;
    let file_size = 36 + data_size;

    let mut wav = Vec::with_capacity(44 + all_pcm.len() * 2);
    wav.extend_from_slice(&header[..4]);   // RIFF
    wav.extend_from_slice(&file_size.to_le_bytes()); // actual size
    wav.extend_from_slice(&header[8..40]); // WAVEfmt...data
    wav.extend_from_slice(&data_size.to_le_bytes()); // actual data size
    for &sample in &all_pcm {
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    (
        [
            (axum::http::header::CONTENT_TYPE, "audio/wav"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"imbe_test.wav\"",
            ),
        ],
        wav,
    )
}

// ── Phase 7B: Monitor list ─────────────────────────────────────────────

/// GET /api/monitor -- return the current monitor list.
/// PUT /api/monitor -- replace the list. Body: {"talkgroups": [300, 402]}
/// GET /api/monitor?add=300 / ?remove=300 -- quick add/remove.
async fn get_monitor(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let mut list = state.monitor_list.write().await;

    if let Some(tg_str) = params.get("add") {
        if let Ok(tg) = tg_str.parse::<u16>() {
            list.add(tg);
        }
    }
    if let Some(tg_str) = params.get("remove") {
        if let Ok(tg) = tg_str.parse::<u16>() {
            list.remove(tg);
        }
    }

    Json(serde_json::json!({
        "talkgroups": list.list(),
    }))
}

async fn put_monitor(
    State(state): State<Arc<AppState>>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut list = state.monitor_list.write().await;
    if let Some(arr) = body.get("talkgroups").and_then(|v| v.as_array()) {
        let tgs: Vec<u16> = arr
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u16))
            .collect();
        list.set(tgs);
    }
    Json(serde_json::json!({
        "talkgroups": list.list(),
    }))
}

// ── Phase 7F.1 (2026-04-14): Event log tail ──────────────────────────

/// GET /api/log -- tail the structured event log.
///   ?since=N   -- return entries with seq > N (default 0 = all)
///   ?limit=N   -- return at most N entries (default 200, cap 1000)
///   ?category=grant|traffic|imbe|vocoder|system
///              -- server-side filter (optional; dashboard also
///              filters client-side so the ring is one source of truth)
///
/// Response shape:
/// ```json
/// {
///   "last_seq": 12345,
///   "count":    42,
///   "entries":  [ { "seq": ..., "timestamp_ms": ..., "category": ...,
///                    "message": ..., "fields": { ... } }, ... ]
/// }
/// ```
async fn get_event_log(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let since = params
        .get("since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .min(1000);
    let category_filter = params.get("category").map(|s| s.to_string());

    let mut entries = state.event_log.recent_since(since, limit);
    if let Some(cat) = &category_filter {
        entries.retain(|e| e.category == cat);
    }
    let last_seq = state.event_log.last_seq();
    Json(serde_json::json!({
        "last_seq": last_seq,
        "count":    entries.len(),
        "entries":  entries,
    }))
}

// ── Phase 7F.4 (2026-04-14): NID batch capture + runtime BCH-t ──────

/// GET /api/nid_capture -- batch NID capture tail for offline BCH
/// analysis.
///
/// Query params:
///   side      = "control" (default) | "traffic"
///   arm       = 1 to arm the ring, 0 to disarm
///   limit     = ring size when arming (default 256, capped at 1024)
///   clear     = 1 to drain the ring and disarm (one-shot readout)
///
/// Read flow:
///   1. `?arm=1&limit=256`  — arm the ring, return {armed:true, limit:256}
///   2. Wait 10-30 s for real traffic to populate the ring
///   3. `?clear=1`          — drain + disarm, returns the full batch
///
/// Offline analysis tool: tools/p25_nid_analyze.py.
async fn get_nid_capture(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let side = params
        .get("side")
        .cloned()
        .unwrap_or_else(|| "control".to_string());
    let arm = params.get("arm").map(String::as_str) == Some("1");
    let clear = params.get("clear").map(String::as_str) == Some("1");
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(256)
        .min(1024);

    // Pick which decoder's ring to hit. `lsm_decoder` for control side
    // (the NAC-healthy one), `traffic_lsm_decoder` for the DUID-broken
    // side that actually matters for audio.
    let decoder_lock = match side.as_str() {
        "traffic" => state.traffic_lsm_decoder.clone(),
        _         => state.lsm_decoder.clone(),
    };

    let mut dec = decoder_lock.write().await;

    let mut action = Vec::new();
    if clear {
        let entries: Vec<_> = dec.drain_capture_ring()
            .into_iter()
            .map(|e| e.to_json())
            .collect();
        action.push("cleared".to_string());
        return Json(serde_json::json!({
            "side":    side,
            "action":  action,
            "count":   entries.len(),
            "entries": entries,
        }));
    }
    if arm {
        dec.arm_capture_ring(limit);
        action.push(format!("armed limit={}", limit));
    }
    let count = dec.capture_ring.len();
    let armed = dec.capture_ring_armed;
    let cap_limit = dec.capture_ring_limit;
    let bch_t = dec.bch_t_override;
    // Snapshot without draining so the client can poll mid-run.
    let entries: Vec<_> = dec.snapshot_capture_ring()
        .into_iter()
        .map(|e| e.to_json())
        .collect();
    Json(serde_json::json!({
        "side":    side,
        "action":  action,
        "armed":   armed,
        "limit":   cap_limit,
        "count":   count,
        "bch_t_override": bch_t,
        "entries": entries,
    }))
}

/// GET /api/bch_t -- read current BCH-t override for both decoder sides.
async fn get_bch_t(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let control = state.lsm_decoder.read().await.bch_t_override;
    let traffic = state.traffic_lsm_decoder.read().await.bch_t_override;
    Json(serde_json::json!({
        "control":  control,
        "traffic":  traffic,
        "default":  crate::lsm::nid_fec::T_MAX_ERRORS,
        "note":     "null = default (T_MAX_ERRORS = 11). Set via \
                     PUT /api/bch_t?side=control|traffic|both&value=N \
                     where N is 0..=11. value=reset or value=null to \
                     clear the override.",
    }))
}

/// PUT /api/bch_t -- set runtime BCH-t override.
///
/// Query params:
///   side   = "control" | "traffic" | "both" (default "traffic")
///   value  = 0..=11 sets the override; "reset" | "null" clears it
///
/// Lower values reject marginal NIDs earlier instead of letting the ML
/// codebook search return a "corrected" word from far away in Hamming
/// space. On a noisy signal the default threshold of 11 often pulls
/// the result toward the all-ones codeword (DUID 0xF = TDU_LC),
/// explaining the `tdu_lc >> ldu1+ldu2` inversion we see on the traffic
/// side. Sweep with tools/p25_nid_analyze.py.
async fn put_bch_t(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let side = params
        .get("side")
        .cloned()
        .unwrap_or_else(|| "traffic".to_string());
    let value_str = params.get("value").cloned().unwrap_or_default();
    let new_override: Option<u32> = if value_str == "reset"
        || value_str == "null"
        || value_str.is_empty()
    {
        None
    } else {
        match value_str.parse::<u32>() {
            Ok(v) if v <= crate::lsm::nid_fec::T_MAX_ERRORS => Some(v),
            Ok(v) => {
                return Json(serde_json::json!({
                    "error": format!(
                        "value={} out of range (max = {})",
                        v, crate::lsm::nid_fec::T_MAX_ERRORS,
                    ),
                }));
            }
            Err(_) => {
                return Json(serde_json::json!({
                    "error": format!(
                        "value={:?} not an integer or 'reset'",
                        value_str,
                    ),
                }));
            }
        }
    };

    let mut applied = Vec::new();
    if side == "control" || side == "both" {
        state.lsm_decoder.write().await.set_bch_t_override(new_override);
        applied.push("control".to_string());
    }
    if side == "traffic" || side == "both" {
        state.traffic_lsm_decoder.write().await.set_bch_t_override(new_override);
        applied.push("traffic".to_string());
    }
    state.event_log.push(
        crate::event_log::LogCategory::System,
        format!(
            "bch_t_override set: side={} value={:?}",
            side, new_override,
        ),
        serde_json::json!({
            "side":  side,
            "value": new_override,
        }),
    );
    Json(serde_json::json!({
        "applied": applied,
        "value":   new_override,
    }))
}

// ── Phase 7F.5 (2026-04-14): manual encryption blocklist ───────────

/// GET /api/encrypted_tgs -- read the current encryption blocklist.
/// Returns the sorted list of TGs in
/// `ImbeForwarder.encrypted_tg_history`.
async fn get_encrypted_tgs(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let mut list: Vec<u16> = state
        .imbe_forwarder
        .encrypted_tg_history
        .lock()
        .map(|h| h.iter().copied().collect())
        .unwrap_or_default();
    list.sort_unstable();
    Json(serde_json::json!({
        "count":  list.len(),
        "tgs":    list,
        "note":   "TGs in this list are permanently rejected by the \
                   grant follower. Populated eagerly by the follower \
                   whenever it observes a grant with encrypted=true, \
                   and manually via ?add=N or ?remove=N. Clear all \
                   via ?clear=1. Persists for the lifetime of the \
                   p25-httpd process only (resets on reboot).",
    }))
}

/// PUT /api/encrypted_tgs -- mutate the blocklist.
///
/// Query params (all optional, multiple can be combined):
///   add    = NNN     -- add TG NNN to the blocklist
///   remove = NNN     -- remove TG NNN from the blocklist
///   clear  = 1       -- clear the whole blocklist
///
/// Motivation: on P25 sites that don't consistently set the
/// service_options `encrypted` bit on every GroupVoiceChannelGrant
/// TSBK, the eager-history populate can never block a TG because
/// we never see the flag. Phase 7F.5 adds a manual override so the
/// operator can say "TG 402 is encrypted on this site, trust me"
/// and the follower will skip it thereafter.
async fn put_encrypted_tgs(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    if params.get("clear").map(String::as_str) == Some("1") {
        if let Ok(mut h) = state.imbe_forwarder.encrypted_tg_history.lock() {
            let n = h.len();
            h.clear();
            applied.push(format!("cleared ({} entries)", n));
        }
    }
    if let Some(v) = params.get("add") {
        match v.parse::<u16>() {
            Ok(tg) => {
                if let Ok(mut h) =
                    state.imbe_forwarder.encrypted_tg_history.lock()
                {
                    if h.insert(tg) {
                        applied.push(format!("added TG={}", tg));
                    } else {
                        applied.push(format!("TG={} already present", tg));
                    }
                }
            }
            Err(_) => errors.push(format!("add={:?} not an integer", v)),
        }
    }
    if let Some(v) = params.get("remove") {
        match v.parse::<u16>() {
            Ok(tg) => {
                if let Ok(mut h) =
                    state.imbe_forwarder.encrypted_tg_history.lock()
                {
                    if h.remove(&tg) {
                        applied.push(format!("removed TG={}", tg));
                    } else {
                        applied.push(format!("TG={} not in list", tg));
                    }
                }
            }
            Err(_) => errors.push(format!("remove={:?} not an integer", v)),
        }
    }

    // If anything changed, also force-idle the follower if it's
    // currently locked on a newly-blocked TG. Otherwise the manual
    // add takes effect only for the NEXT grant for that TG.
    let currently_locked = state
        .traffic_manager
        .lock()
        .await
        .current_talkgroup()
        .map(|t| t.0);
    if let Some(locked_tg) = currently_locked {
        let blocked_now = state
            .imbe_forwarder
            .encrypted_tg_history
            .lock()
            .map(|h| h.contains(&locked_tg))
            .unwrap_or(false);
        if blocked_now {
            let mut mgr = state.traffic_manager.lock().await;
            mgr.force_idle();
            drop(mgr);
            use std::sync::atomic::Ordering;
            state
                .imbe_forwarder
                .current_talkgroup
                .store(0, Ordering::Relaxed);
            #[cfg(target_os = "linux")]
            {
                let core = state.ip_core.lock().await;
                core.set_traffic_demod_enable(false);
            }
            {
                let mut dec = state.traffic_lsm_decoder.write().await;
                dec.reset_framer_state();
            }
            applied.push(format!(
                "force-idle: was locked on TG={} which is now blocked",
                locked_tg,
            ));
        }
    }

    state.event_log.push(
        crate::event_log::LogCategory::System,
        format!("encrypted_tgs update: {}", applied.join(", ")),
        serde_json::json!({
            "applied": applied.clone(),
            "errors":  errors.clone(),
        }),
    );
    let list: Vec<u16> = {
        let mut v: Vec<u16> = state
            .imbe_forwarder
            .encrypted_tg_history
            .lock()
            .map(|h| h.iter().copied().collect())
            .unwrap_or_default();
        v.sort_unstable();
        v
    };
    Json(serde_json::json!({
        "applied": applied,
        "errors":  errors,
        "count":   list.len(),
        "tgs":     list,
    }))
}

// ── Phase 7E: Audio streaming ─────────────────────────────────────────

/// GET /api/audio -- stream PCM audio.
///   ?format=wav  -- prepend a WAV header (default: raw PCM)
///   Content-Type: audio/L16;rate=8000;channels=1 (raw) or audio/wav
///
/// The response is a chunked HTTP stream that runs until the client
/// disconnects. Each chunk is 320 bytes (160 samples * 2 bytes).
/// Pipe to `aplay -r 8000 -f S16_LE -c 1` or open in VLC.
async fn get_audio(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    let format = params
        .get("format")
        .map(String::as_str)
        .unwrap_or("raw");
    let want_wav = format == "wav";
    let mut rx = state.audio_tx.subscribe();

    let stream = async_stream::stream! {
        if want_wav {
            yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(
                &crate::audio::wav_header_8k_16bit_mono()
            ));
        }
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    let mut buf = [0u8; 320];
                    for (i, &sample) in chunk.pcm.iter().enumerate() {
                        let le = sample.to_le_bytes();
                        buf[i * 2] = le[0];
                        buf[i * 2 + 1] = le[1];
                    }
                    yield Ok(bytes::Bytes::copy_from_slice(&buf));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    let content_type = if want_wav {
        "audio/wav"
    } else {
        "audio/L16;rate=8000;channels=1"
    };

    (
        [(axum::http::header::CONTENT_TYPE, content_type)],
        axum::body::Body::from_stream(stream),
    )
}

/// WebSocket /ws/audio -- binary frames of 320 bytes (160 i16 LE
/// samples = 20 ms of 8 kHz mono audio per message).
async fn ws_audio(
    ws: axum::extract::WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_audio(socket, state))
}

async fn handle_ws_audio(
    mut socket: axum::extract::ws::WebSocket,
    state: Arc<AppState>,
) {
    use std::sync::atomic::Ordering;
    let mut rx = state.audio_tx.subscribe();
    loop {
        match rx.recv().await {
            Ok(chunk) => {
                let mut buf = [0u8; 320];
                for (i, &sample) in chunk.pcm.iter().enumerate() {
                    let le = sample.to_le_bytes();
                    buf[i * 2] = le[0];
                    buf[i * 2 + 1] = le[1];
                }
                if socket
                    .send(axum::extract::ws::Message::Binary(buf.to_vec().into()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                // The broadcast channel dropped `skipped` chunks because
                // this consumer fell behind. Each lagged chunk is a gap
                // the listener will hear. Bump the global counter so
                // /api/stats.audio_ws_lag_total reflects it and the
                // dashboard can distinguish this (server-side loss) from
                // browser-side jitter-buffer underruns.
                state.audio_ws_lag_total.fetch_add(skipped, Ordering::Relaxed);
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

// ── Dashboard HTML ─────────────────────────────────────────────────────

async fn index_html() -> impl IntoResponse {
    axum::response::Html(DASHBOARD_HTML)
}

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Fishball P25</title>
<style>
:root {
  --bg: #0a0a0f;
  --card-bg: #12122a;
  --card-border: #2a2a4a;
  --text: #d0d0e0;
  --text-dim: #707090;
  --accent: #4fc3f7;
  --green: #66bb6a;
  --orange: #ffb74d;
  --red: #ef5350;
  --purple: #ab47bc;
  --mono: 'Cascadia Code', 'Fira Code', 'JetBrains Mono', monospace;
}
[data-theme="light"] {
  --bg: #f5f5f5;
  --card-bg: #ffffff;
  --card-border: #ddd;
  --text: #222;
  --text-dim: #888;
  --accent: #0277bd;
  --green: #2e7d32;
  --orange: #e65100;
  --red: #c62828;
  --purple: #7b1fa2;
}
* { margin: 0; padding: 0; box-sizing: border-box; }
body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif;
       background: var(--bg); color: var(--text); padding: 12px; font-size: 14px; }
h1 { color: var(--accent); font-size: 1.3em; }
h2 { color: var(--green); margin: 12px 0 6px; font-size: 1em; font-weight: 600; }
.header { display: flex; align-items: center; justify-content: space-between; margin-bottom: 12px; }
.header-left { display: flex; align-items: center; gap: 12px; }
.status { display: inline-flex; align-items: center; gap: 6px; font-size: 0.85em; }
.dot { width: 8px; height: 8px; border-radius: 50%; background: var(--red); }
.dot.active { background: var(--green); }
.theme-btn { background: none; border: 1px solid var(--card-border); color: var(--text);
              padding: 4px 10px; border-radius: 4px; cursor: pointer; font-size: 0.85em; }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 10px; }
.card { background: var(--card-bg); border: 1px solid var(--card-border);
        border-radius: 6px; padding: 10px; }
table { width: 100%; border-collapse: collapse; font-size: 0.85em; }
th { text-align: left; color: var(--text-dim); padding: 3px 6px; border-bottom: 1px solid var(--card-border); font-weight: 500; }
td { padding: 3px 6px; border-bottom: 1px solid rgba(128,128,128,0.1); }
.v { color: var(--accent); font-family: var(--mono); font-size: 0.9em; }
.freq { color: var(--orange); }
.tg { color: var(--green); }
.alias { color: var(--purple); font-size: 0.8em; }

/* Activity filter bar */
.activity-filters { display: flex; flex-wrap: wrap; gap: 6px 14px; margin: 8px 0; font-size: 0.8em; }
.af { display: flex; align-items: center; gap: 4px; cursor: pointer; user-select: none; }
.af input { margin: 0; cursor: pointer; }
.af-swatch { display: inline-block; width: 10px; height: 10px; border-radius: 2px; }

/* Live activity feed */
#activity { max-height: 400px; overflow-y: auto; font-family: var(--mono); font-size: 0.8em;
            display: flex; flex-direction: column-reverse; }
.evt { padding: 3px 6px; border-bottom: 1px solid rgba(128,128,128,0.08); display: flex; gap: 8px; flex-shrink: 0; }
.evt-time { color: var(--text-dim); min-width: 80px; }
.evt-type { min-width: 80px; font-weight: 600; }
.evt-type.GRP_GRANT { color: var(--green); }
.evt-type.GRANT_UPD { color: var(--accent); }
.evt-type.NET_STS, .evt-type.RFSS_STS { color: var(--orange); }
.evt-type.IDEN_UP { color: var(--purple); }
.evt-type.ADJ_STS { color: var(--text-dim); }
.evt-type.SCCB { color: var(--text-dim); }
.evt-type.SNDCP_ANN { color: var(--text-dim); }
.evt-type.TDMA_SYNC { color: var(--text-dim); }
.evt-type.TEL_INT_GRANT_UPD { color: var(--accent); }
.evt-type.UU_ANS_REQ { color: var(--text); }
.evt-type.TRF_HDU { color: #4fc3f7; }
.evt-type.TRF_LDU1, .evt-type.TRF_LDU2 { color: #4db6ac; }
.evt-type.TRF_TDU, .evt-type.TRF_TDU_LC { color: #ffb74d; }
.evt-detail { flex: 1; }

/* Frequency map */
.freq-map { position: relative; height: 60px; background: var(--card-bg);
            border: 1px solid var(--card-border); border-radius: 6px;
            margin: 8px 0; overflow: hidden; }
.freq-marker { position: absolute; bottom: 0; width: 2px; height: 100%;
               background: var(--text-dim); opacity: 0.4; }
.freq-marker.cc { background: var(--accent); opacity: 0.8; width: 3px; }
.freq-marker.active { background: var(--green); opacity: 0.9; width: 3px; }
.freq-label { position: absolute; top: 2px; font-size: 9px; color: var(--text-dim);
              font-family: var(--mono); transform: translateX(-50%); white-space: nowrap; }
.freq-lcn { position: absolute; bottom: 2px; font-size: 9px; color: var(--accent);
            font-family: var(--mono); transform: translateX(-50%); }

/* Aliases modal */
.modal-overlay { display: none; position: fixed; top: 0; left: 0; width: 100%; height: 100%;
                 background: rgba(0,0,0,0.6); z-index: 100; }
.modal-overlay.show { display: flex; align-items: center; justify-content: center; }
.modal { background: var(--card-bg); border: 1px solid var(--card-border);
         border-radius: 8px; padding: 16px; width: 500px; max-width: 90vw; }
.modal h2 { margin-top: 0; }
.modal textarea { width: 100%; height: 200px; background: var(--bg); color: var(--text);
                  border: 1px solid var(--card-border); border-radius: 4px; padding: 8px;
                  font-family: var(--mono); font-size: 0.85em; resize: vertical; }
.modal-btns { display: flex; gap: 8px; margin-top: 8px; justify-content: flex-end; }
.btn { padding: 6px 14px; border: 1px solid var(--card-border); border-radius: 4px;
       cursor: pointer; font-size: 0.85em; background: var(--card-bg); color: var(--text); }
.btn-primary { background: var(--accent); color: #000; border-color: var(--accent); }
.alias-btn { font-size: 0.8em; color: var(--text-dim); cursor: pointer; margin-left: 8px; }
/* Live-audio bar (Phase 7E UI hookup) */
.audio-bar { display: flex; align-items: center; gap: 10px; flex-wrap: wrap;
             background: var(--card-bg); border: 1px solid var(--card-border);
             border-radius: 6px; padding: 8px 12px; margin: 6px 0 10px; }
.audio-bar .btn.play-on { background: var(--green); border-color: var(--green); color:#000; }
.audio-bar .btn.play-off { background: var(--red); border-color: var(--red); color:#000; }
.audio-bar label { font-size: 0.85em; color: var(--text-dim); display:flex;
                   align-items:center; gap:6px; }
.audio-bar input[type=range] { width: 120px; vertical-align: middle; }
.audio-bar .status { font-family: var(--mono); font-size: 0.82em; color: var(--text-dim);
                     margin-left: auto; }
.audio-bar .status .on { color: var(--green); }
.audio-bar .status .warn { color: var(--orange); }
.audio-bar .status .err { color: var(--red); }
/* Tab nav (2026-04-14 split: operator "Radio" view vs FPGA "Debug" view) */
.tab-nav { display: flex; gap: 4px; margin: 6px 0 16px;
           border-bottom: 1px solid var(--card-border); }
.tab-nav button { background: transparent; border: 1px solid transparent;
                  border-bottom: none; padding: 8px 18px; cursor: pointer;
                  font-size: 0.95em; color: var(--text-dim);
                  border-top-left-radius: 6px; border-top-right-radius: 6px;
                  margin-bottom: -1px; font-family: inherit; }
.tab-nav button:hover { color: var(--text); }
.tab-nav button.active { background: var(--card-bg); color: var(--accent);
                         border-color: var(--card-border);
                         border-bottom: 1px solid var(--card-bg); }
.tab-nav button .tab-badge { font-size: 0.75em; color: var(--text-dim);
                             margin-left: 6px; font-family: var(--mono); }
.tab-pane { display: none; }
.tab-pane.active { display: block; }
/* Logs tab (2026-04-14): structured event-log viewer */
.log-toolbar { display: flex; align-items: center; gap: 10px; flex-wrap: wrap;
               margin: 6px 0 10px; padding: 8px 12px;
               background: var(--card-bg); border: 1px solid var(--card-border);
               border-radius: 6px; }
.log-toolbar label { font-size: 0.85em; color: var(--text-dim); display: flex;
                     align-items: center; gap: 4px; cursor: pointer; }
.log-toolbar label input { cursor: pointer; }
.log-toolbar .log-status { margin-left: auto; font-family: var(--mono);
                           font-size: 0.82em; color: var(--text-dim); }
.log-viewer { font-family: var(--mono); font-size: 0.80em;
              background: var(--bg); border: 1px solid var(--card-border);
              border-radius: 6px; padding: 10px; max-height: 70vh;
              overflow-y: auto; white-space: pre-wrap; word-break: break-word; }
.log-entry { display: grid; grid-template-columns: 110px 90px 1fr;
             gap: 8px; padding: 3px 0; border-bottom: 1px dotted var(--card-border); }
.log-entry:last-child { border-bottom: none; }
.log-entry .log-ts { color: var(--text-dim); }
.log-entry .log-cat { font-weight: 600; }
.log-entry .log-cat.grant   { color: var(--green); }
.log-entry .log-cat.traffic { color: var(--accent); }
.log-entry .log-cat.imbe    { color: #4db6ac; }
.log-entry .log-cat.vocoder { color: var(--orange); }
.log-entry .log-cat.system  { color: var(--purple); }
.log-entry .log-body .log-fields { color: var(--text-dim); margin-left: 6px; font-size: 0.92em; }
@media (max-width: 768px) { .grid2 { grid-template-columns: 1fr; } }
</style>
</head>
<body>

<div class="header">
  <div class="header-left">
    <h1>&#x1f4e1; Fishball P25</h1>
    <span class="status"><span class="dot" id="dot"></span><span id="status">Offline</span></span>
    <span style="font-size:0.75em;color:var(--text-dim);font-family:var(--mono)" id="build_tag">--</span>
  </div>
  <div>
    <span class="alias-btn" onclick="showAliases()">&#x2699; Aliases</span>
    <button class="theme-btn" onclick="toggleTheme()" id="themeBtn">&#x1f319;</button>
  </div>
</div>

<!-- Tab bar: Radio (operator) / Debug (FPGA bring-up). Switching
     is pure display:none toggle; all panes stay in the DOM so the
     2 s refresh loop updates everything regardless of which tab
     the user is on. Default tab is persisted in localStorage. -->
<div class="tab-nav" id="tabNav">
  <button data-tab="radio" class="active" onclick="switchTab('radio')">&#x1f4fb; Radio</button>
  <button data-tab="logs" onclick="switchTab('logs')">&#x1f4dc; Logs <span class="tab-badge" id="logs_badge"></span></button>
  <button data-tab="debug" onclick="switchTab('debug')">&#x1f527; Debug</button>
  <button data-tab="api" onclick="switchTab('api')">&#x1f4d6; API</button>
</div>

<!-- ═════════════════════ API tab ═════════════════════ -->
<div class="tab-pane" id="tab-api">
  <h2>HTTP API reference</h2>
  <p style="color:var(--text-dim);font-size:0.9em">
    Machine-readable list of every `/api/*` route served by this
    p25-httpd instance. Sourced from the hand-maintained
    `/api/endpoints` catalogue in `httpd/mod.rs::ENDPOINT_CATALOGUE`;
    see that constant for the source-of-truth comments.
  </p>
  <div class="card">
    <input type="text" id="api_filter" placeholder="filter by path or keyword (e.g. 'traffic', 'retune', 'dump')"
      style="width:100%;padding:6px 8px;margin-bottom:8px;font-family:inherit" />
    <table id="api_table" style="width:100%;font-size:0.85em">
      <thead>
        <tr>
          <th style="width:5em">Method</th>
          <th style="width:18em">Path</th>
          <th style="width:14em">Query params</th>
          <th>Description</th>
          <th style="width:3em">Try</th>
        </tr>
      </thead>
      <tbody id="api_tbody">
        <tr><td colspan="5" style="color:var(--text-dim)">Loading /api/endpoints...</td></tr>
      </tbody>
    </table>
  </div>
</div>

<!-- ═════════════════════ Debug tab ═════════════════════ -->
<div class="tab-pane" id="tab-debug">

<!-- ── IQ Constellation (2026-04-16) ────────────────────────── -->
<h2>IQ Constellation <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">post-PLL symbol decision points</span></h2>
<div class="card">
  <div style="display:flex;flex-wrap:wrap;align-items:center;gap:10px;margin-bottom:6px;font-size:0.85em">
    <label><b>Chain</b>:
      <select id="iq_chain" onchange="refreshConstellation()" style="padding:2px 6px;font-family:inherit">
        <option value="traffic" selected>Traffic</option>
        <option value="control">Control</option>
      </select>
    </label>
    <label><b>Poll</b>:
      <select id="iq_rate" onchange="scheduleConstellation()" style="padding:2px 6px;font-family:inherit">
        <option value="500">2 Hz</option>
        <option value="1000" selected>1 Hz</option>
        <option value="2000">0.5 Hz</option>
        <option value="0">Paused</option>
      </select>
    </label>
    <label><input type="checkbox" id="iq_persistence" checked> <b>Persistence (fade)</b></label>
    <span class="v" id="iq_status" style="font-size:0.85em;color:var(--text-dim)">idle</span>
  </div>
  <canvas id="iq_canvas" width="480" height="480"
    style="width:480px;height:480px;background:#0a0f1a;border:1px solid #1f2937;display:block;margin:0 auto"></canvas>
  <div style="font-size:0.75em;color:var(--text-dim);margin-top:6px;text-align:center">
    Post-PLL decision-time (I, Q) points from the Phase 6D software LSM demod.
    Clean LSM signal = 4 tight clusters near ±1, ±j. Phase rotation ⇒
    tilt; amplitude compression ⇒ radial shrinkage toward the center;
    noise ⇒ cloud spread. Updates stop when the tab is inactive.
  </div>
</div>

<!-- ── RF Spectrum (narrowband, 2026-04-16) ─────────────────── -->
<h2>RF Spectrum <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">post-DDC, ±31.25 kHz</span></h2>
<div class="card">
  <div style="display:flex;flex-wrap:wrap;align-items:center;gap:10px;margin-bottom:6px;font-size:0.85em">
    <label><b>Chain</b>:
      <select id="spec_chain" onchange="refreshSpectrum()" style="padding:2px 6px;font-family:inherit">
        <option value="control">Control</option>
        <option value="traffic">Traffic</option>
      </select>
    </label>
    <label><b>Poll</b>:
      <select id="spec_rate" onchange="scheduleSpectrum()" style="padding:2px 6px;font-family:inherit">
        <option value="500">2 Hz</option>
        <option value="1000" selected>1 Hz</option>
        <option value="2000">0.5 Hz</option>
        <option value="0">Paused</option>
      </select>
    </label>
    <label><input type="checkbox" id="spec_peak_hold"> <b>Peak-hold</b></label>
    <button class="btn" style="padding:2px 10px" onclick="resetSpectrum()">Reset</button>
    <span class="v" id="spec_status" style="font-size:0.85em;color:var(--text-dim)">idle</span>
  </div>
  <canvas id="spec_canvas" width="900" height="240"
    style="width:100%;height:240px;background:#0a0f1a;border:1px solid #1f2937"></canvas>
  <div style="font-size:0.75em;color:var(--text-dim);margin-top:6px">
    Narrowband FFT (4096 pts) over the post-DDC IQ ring, centered on
    the selected chain's channel. Span = ±31.25 kHz, bin width ≈ 15 Hz.
    Useful for channel-shape verification, adjacent-channel
    interference, DC-blocker residuals. Wideband view (full 8 MHz
    around LO) is a future HDL addition; this software FFT works on
    any bitstream that has the iq_dma ring.
  </div>
</div>

<!-- ── Decoder Comparison Matrix (2-column PS LSM / PL HDL view) ── -->
<!-- 2026-04-16: PS C4FM column removed. The HDL LSM chain decodes
     both C4FM and LSM sites so the PS C4FM pipeline is dormant
     everywhere. -->
<h2>Decoder Comparison (PS framer vs PL gateware)</h2>
<div class="card">
  <table id="cmp_t" style="font-size:0.85em">
    <thead>
      <tr>
        <th style="width:40%">Metric</th>
        <th>PS LSM framer<br><span style="color:var(--text-dim);font-weight:400">software framer, PL HDL LSM dibit-fed (production)</span></th>
        <th>PL HDL LSM<br><span style="color:var(--text-dim);font-weight:400">FPGA gateware (heartbeat snapshot)</span></th>
      </tr>
    </thead>
    <tbody id="cmp_body">
      <tr><td colspan="3" style="color:var(--text-dim)">Loading...</td></tr>
    </tbody>
  </table>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
    PS = Processing System (ARM software framer on top of PL-emitted
    dibits). PL = Programmable Logic (FPGA gateware). "(PS only)"
    marks rows with no PL equivalent by design (PL is a NID decoder,
    not a TSBK framer). "(= PS)" marks rows where the PS column is
    the authoritative counter for a value that's actually generated
    in the PL. "(HDL: hit-only)" marks the sync near-miss row — the
    HDL hard-sync correlator only fires when Hamming distance ≤
    threshold, so it doesn't count misses.
  </p>
</div>

<div class="grid2">
  <div class="card">
    <h2>System Identity <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">PS &middot; LSM software decoder &middot; HDL dibit-fed</span></h2>
    <table>
      <tr><th>NAC</th><td class="v" id="nac">--</td></tr>
      <tr><th>WACN</th><td class="v" id="wacn">--</td></tr>
      <tr><th>System</th><td class="v" id="sys">--</td></tr>
      <tr><th>RFSS / Site</th><td class="v" id="rfss">--</td></tr>
      <tr><th>Control CH</th><td class="v" id="cc">--</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Software decoder fed by HDL LSM dibit DMA. Empty until a TSBK validates.
    </p>
  </div>
  <div class="card">
    <h2>Decode Stats <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">PS &middot; LSM software decoder &middot; HDL dibit-fed</span></h2>
    <table>
      <tr><th>Messages</th><td class="v" id="msgs">0</td></tr>
      <tr><th>Active Grants</th><td class="v" id="grants_n">0</td></tr>
      <tr><th>Bands Known</th><td class="v" id="bands_n">0</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Decoder counters from the PS LSM software decoder running on
      PL HDL LSM dibits (production).
    </p>
    <!-- `dibits`/`overflow` element ids are still referenced by
         refresh() for historical reasons -- keep them hidden so
         the JS doesn't throw on missing elements. Harmless once
         refresh() is rewritten to drop the dibits assignment,
         which is a future cleanup. -->
    <span id="dibits" style="display:none">0</span>
    <span id="overflow" style="display:none">No</span>
  </div>
</div>

<!-- ── Phase 6F.2: PL HDL LSM Chain Detail + IRQ Counters ── -->
<div class="grid2">
  <div class="card">
    <h2>HDL LSM Chain (PL) <span id="hdl_status" style="font-size:0.75em;color:var(--text-dim);margin-left:6px">--</span></h2>
    <table style="font-size:0.85em">
      <tr><th>Cumulative NIDs (valid/total)</th><td class="v"><span id="hdl_nid_valid">0</span> / <span id="hdl_nid_total">0</span> (<span id="hdl_nid_pct">0%</span>)</td></tr>
      <tr><th>Last NAC / DUID</th><td class="v"><span id="hdl_last_nac">--</span> / <span id="hdl_last_duid">--</span></td></tr>
      <tr><th>Drop count (sync hit while BCH busy)</th><td class="v" id="hdl_drop">0</td></tr>
      <tr><th>PLL register (now)</th><td class="v" id="hdl_pll">--</td></tr>
      <tr><th>Sample point register (now)</th><td class="v" id="hdl_sp">--</td></tr>
      <tr><th>Sync distance (now / window best)</th><td class="v"><span id="hdl_sd_now">--</span> / <span id="hdl_sd_best">--</span></td></tr>
      <tr><th>BCH busy / in-NID-window flags</th><td class="v"><span id="hdl_bch">--</span> / <span id="hdl_inwin">--</span></td></tr>
      <tr><th>Last 1s window: pll min/max</th><td class="v"><span id="hdl_w_pll">--</span></td></tr>
      <tr><th>Last 1s window: sp min/max</th><td class="v"><span id="hdl_w_sp">--</span></td></tr>
      <tr><th>Last 1s window: NIDs (valid/total)</th><td class="v"><span id="hdl_w_nids">--</span></td></tr>
      <tr><th>Last 1s window: iq KB/s, buf rolls</th><td class="v"><span id="hdl_w_iq">--</span></td></tr>
      <tr><th>Cumulative dibit / iq overflow ticks</th><td class="v"><span id="hdl_ovf">0 / 0</span></td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Live FPGA register reads from the heartbeat task (16 ms cadence).
      pll_dbg / sp_dbg are signed 16-bit. Healthy lock for our test target:
      pll near 0, sp ~3000-6000, sync_dist 0.
    </p>
  </div>
  <div class="card">
    <h2>IRQ Source Counters</h2>
    <table style="font-size:0.85em">
      <tr><th>Total IRQs</th><td class="v"><span id="irq_total">0</span> (<span id="irq_total_rate">0/s</span>)</td></tr>
      <tr><th>C4FM dibit DMA done</th><td class="v"><span id="irq_dibit">0</span> (<span id="irq_dibit_rate">0/s</span>)</td></tr>
      <tr><th>Traffic dibit DMA done</th><td class="v"><span id="irq_traffic">0</span> (<span id="irq_traffic_rate">0/s</span>)</td></tr>
      <tr><th>IQ DMA done</th><td class="v"><span id="irq_iq">0</span> (<span id="irq_iq_rate">0/s</span>)</td></tr>
      <tr><th>LSM dibit DMA done</th><td class="v"><span id="irq_lsm">0</span> (<span id="irq_lsm_rate">0/s</span>)</td></tr>
      <tr><th>Last IRQ (ms ago)</th><td class="v" id="irq_last">--</td></tr>
      <tr><th>Uptime</th><td class="v" id="irq_uptime">--</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Per-source IRQ counters from the InterruptHandler task. lsm_dibit
      should be ~17/min (one buffer every ~3.5 s) on a healthy 4800 sym/s
      LSM dibit stream. iq should be ~60/s (one sub-buffer every 16 ms).
    </p>
  </div>
</div>

<!-- ── Phase 6F.2: Last 32 NIDs from PL HDL ring buffer ── -->
<h2>HDL LSM NID Ring (last 32, PL)</h2>
<div class="card">
  <table style="font-size:0.78em">
    <thead>
      <tr>
        <th>#</th><th>t (ms)</th><th>NAC</th><th>DUID</th>
        <th>valid</th><th>n_err</th><th>sync_d</th>
        <th>drop</th><th>pll</th><th>sp</th>
      </tr>
    </thead>
    <tbody id="nid_ring_body">
      <tr><td colspan="10" style="color:var(--text-dim)">No NID events yet</td></tr>
    </tbody>
  </table>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
    32-deep ring buffer of the most recent NID events as observed by the
    HDL LSM chain. Same data the heartbeat task dumps to the log on
    crash transition.
  </p>
</div>

<!-- Phase 9: the "LSM Decoder (Phase 6D)" + "Top NACs (LSM)"
     cards that used to sit here were removed along with the
     Phase 6D software LSM pipeline. The production LSM stats
     now live in the "PL HDL LSM Chain Detail" card above,
     which reads from `/api/hdl_lsm` (the heartbeat-populated
     HdlLsmRuntime struct that taps the FPGA register bank
     directly). -->

<!-- 2026-04-16: PS C4FM dibit diagnostics retired from the dashboard.
     The HDL LSM chain decodes both C4FM and LSM sites (validated on
     FP&L + Clay + Duval) so the PS C4FM path is dormant everywhere.
     The LSM dibit stream card below is what's actually live. -->
<h2>LSM Dibit Stream Diagnostics <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">PL HDL LSM chain (lsm_dibit_dma)</span></h2>
<div class="grid2">
  <div class="card">
    <h2>PS LSM Dibit Stream <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">lsm_dibit_dma</span></h2>
    <table>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Histogram</th></tr>
      <tr><th>Total Dibits</th><td class="v" id="ldh_total">0</td></tr>
      <tr><th>Value 0 (+1)</th><td class="v" id="ldh_0">--</td></tr>
      <tr><th>Value 1 (+3)</th><td class="v" id="ldh_1">--</td></tr>
      <tr><th>Value 2 (-1)</th><td class="v" id="ldh_2">--</td></tr>
      <tr><th>Value 3 (-3)</th><td class="v" id="ldh_3">--</td></tr>
      <tr><th>Inner / Outer ratio</th><td class="v" id="ldh_io">--</td></tr>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Sync correlator</th></tr>
      <tr><th>Sync hits</th><td class="v" id="lsy_hits">0</td></tr>
      <tr><th>Near misses</th><td class="v" id="lsy_near">0</td></tr>
      <tr><th>Best Hamming distance</th><td class="v" id="lsy_best">--</td></tr>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Raw on-air DUID histogram</th></tr>
      <tr><th>Total NIDs</th><td class="v" id="lrd_total">0</td></tr>
      <tr><th>Bucket 7 TSDU %</th><td class="v" id="lrd_7">--</td></tr>
      <tr><th>Bucket 5 LDU1 %</th><td class="v" id="lrd_5">--</td></tr>
      <tr><th>Bucket A LDU2 %</th><td class="v" id="lrd_a">--</td></tr>
      <tr><th>Bucket 0 HDU %</th><td class="v" id="lrd_0">--</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Live metrics from the PL HDL LSM dibit stream (production
      decoder for both LSM and C4FM sites). TSDU bucket &lt;90 % means
      NID payload bits are being corrupted upstream.
    </p>
  </div>
</div>

</div><!-- /#tab-debug -->

<!-- ═════════════════════ Radio tab (default) ═════════════════════ -->
<div class="tab-pane active" id="tab-radio">

<!-- ── Phase 10: Board Info ── -->
<!-- Single-glance health panel: firmware build, uptime, wall clock,
     AD9361 tuning + gain + RSSI + BW, DDC geometry, and /ws/audio
     lag/client counters. All driven off /api/stats (which is already
     polled by the 2 s refresh loop) so adding a second endpoint isn't
     needed. -->
<h2>Board Info</h2>
<div class="card" id="board_info_card">
  <table style="font-size:0.85em">
    <tbody>
      <tr>
        <th style="width:18%">Build</th>
        <td class="v" id="bi_build">--</td>
        <th style="width:18%">Uptime</th>
        <td class="v" id="bi_uptime">--</td>
      </tr>
      <tr>
        <th>Wall clock</th><td class="v" id="bi_clock">--</td>
        <th>NAC / WACN</th><td class="v" id="bi_nac">--</td>
      </tr>
      <tr>
        <th>System type</th><td class="v" id="bi_sys_type">--</td>
        <th>Site (RFSS/Site)</th><td class="v" id="bi_rfss">--</td>
      </tr>
      <tr>
        <th>RX LO</th><td class="v" id="bi_rx_lo">--</td>
        <th>RF BW</th><td class="v" id="bi_rf_bw">--</td>
      </tr>
      <tr>
        <th>Gain / Mode</th><td class="v" id="bi_gain">--</td>
        <th>RSSI</th><td class="v" id="bi_rssi">--</td>
      </tr>
      <tr>
        <th>Sample rate</th><td class="v" id="bi_sr">--</td>
        <th>DDC chain</th><td class="v" id="bi_ddc">--</td>
      </tr>
      <tr>
        <th>Control offset</th><td class="v" id="bi_ddc_off">--</td>
        <th>DDC output</th><td class="v" id="bi_ddc_out">--</td>
      </tr>
      <tr>
        <th>Audio WS clients</th><td class="v" id="bi_ws_clients">--</td>
        <th>Audio WS lag</th><td class="v" id="bi_ws_lag">--</td>
      </tr>
    </tbody>
  </table>
  <!-- Live front-end retune. Fires GET /api/reinit with whatever
       fields the user filled in. Unfilled fields keep their current
       live value (rf_bandwidth from /api/stats, rx_lo from
       /api/system). Control freq, center freq (rx_lo), and BW are
       independently overridable so the user can jump sites, widen /
       narrow the analog filter, or re-center the LO without editing
       boot config. If the new control_freq is outside the LO's
       ±(BW/2) window, auto-nudge rx_lo to keep the NCO in range. -->
  <div style="display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-top:8px;font-size:0.85em">
    <label for="bi_retune_mhz"><b>Control (MHz)</b>:</label>
    <input type="number" id="bi_retune_mhz" step="0.001" placeholder="e.g. 855.4875"
      style="width:11em;font-family:inherit" />
    <label for="bi_center_mhz"><b>Center (MHz)</b>:</label>
    <input type="number" id="bi_center_mhz" step="0.001" placeholder="auto"
      style="width:10em;font-family:inherit" />
    <label for="bi_bw_mhz"><b>BW (MHz)</b>:</label>
    <input type="number" id="bi_bw_mhz" step="0.1" placeholder="keep"
      style="width:7em;font-family:inherit" />
    <button id="bi_retune_btn" class="btn" style="padding:3px 10px" onclick="retuneControl()">Tune</button>
    <span class="v" id="bi_retune_status" style="font-size:0.85em;color:var(--text-dim)">idle</span>
  </div>
  <div style="display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-top:6px;font-size:0.85em">
    <label for="bi_mod_sel"><b>Modulation</b>:</label>
    <select id="bi_mod_sel" onchange="setModulation(this.value)"
      style="padding:3px 6px;font-family:inherit">
      <option value="auto">Auto-detect</option>
      <option value="lsm">LSM (Clay, Duval, Jax Sheriff)</option>
      <option value="c4fm">C4FM (FP&amp;L, St Johns)</option>
    </select>
    <span class="v" id="bi_mod_status" style="font-size:0.85em;color:var(--text-dim)">--</span>
  </div>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
    Pulled from /api/stats every 2 s. Wall clock is the Linux system
    clock — reads as 1970-... until NTP syncs at boot. Audio WS lag is
    the cumulative count of broadcast-channel Lagged events (server
    saw a browser consumer fall behind); non-zero means the listener
    heard a gap. Distinct from the AudioWorklet underrun counter in
    the playback status line below, which is the browser-side ring
    running dry. Tune posts any filled field to /api/reinit
    (control_freq / rx_lo / rf_bandwidth). Control-only retune keeps
    the LO and shifts the DDC NCO. Setting Center moves the LO
    (required when jumping bands &gt; BW/2 away). If you set Control
    alone and the target is outside LO ± BW/2, the Center field is
    filled automatically so the LO follows. Blank fields keep current
    values; blank Control resets to boot default.
  </p>
</div>

<!-- ── Phase 7D: Traffic Channel + Vocoder ── -->
<h2>Traffic Channel <span id="trf_phase" style="font-size:0.75em;color:var(--text-dim);margin-left:6px"></span></h2>
<!-- Phase 7E: browser-side live-audio playback (WS /ws/audio) -->
<div class="audio-bar">
  <button id="audioBtn" class="btn play-off" onclick="toggleAudio()">&#9654; Play Audio</button>
  <label><input type="checkbox" id="audioMute"> Mute</label>
  <label>Vol <input type="range" id="audioVol" min="0" max="100" value="80"></label>
  <span class="status" id="audioStatus">stopped</span>
</div>
<div class="grid2">
  <div class="card">
    <h2>Grant Follower</h2>
    <table>
      <tbody>
        <tr><th>State</th><td class="v" id="trf_state">--</td></tr>
        <tr><th>Current TG</th><td class="v" id="trf_tg">--</td></tr>
        <tr><th>Frequency</th><td class="v" id="trf_freq">--</td></tr>
        <tr><th>Encrypted</th><td class="v" id="trf_enc">--</td></tr>
        <tr><th>Grants Seen</th><td class="v" id="trf_grants">--</td></tr>
        <tr><th>Retunes</th><td class="v" id="trf_retunes">--</td></tr>
        <tr><th>Encrypted Skipped</th><td class="v" id="trf_rej_enc">--</td></tr>
        <tr><th>Last DUID</th><td class="v" id="trf_duid">--</td></tr>
        <tr><th>Last Retune</th><td class="v" id="trf_last_retune">--</td></tr>
      </tbody>
    </table>
  </div>
  <div class="card">
    <h2>IMBE + Vocoder <span id="voc_status" style="font-size:0.75em;color:var(--green);margin-left:6px"></span></h2>
    <table>
      <tbody>
        <tr><th>HDU</th><td class="v" id="trf_hdu">0</td></tr>
        <tr><th>LDU1</th><td class="v" id="trf_ldu1">0</td></tr>
        <tr><th>LDU2</th><td class="v" id="trf_ldu2">0</td></tr>
        <tr><th>TDU / TDU_LC</th><td class="v" id="trf_tdu">0 / 0</td></tr>
        <tr><th>IMBE Extracted</th><td class="v" id="trf_imbe">0</td></tr>
        <tr><th>IMBE Expected</th><td class="v" id="trf_imbe_exp">0</td></tr>
        <tr><th>IMBE Dropped</th><td class="v" id="trf_imbe_drop">0</td></tr>
        <tr><th>Dropped Idle (phantom)</th><td class="v" id="trf_imbe_drop_idle">0</td></tr>
        <tr><th>Last IMBE</th><td class="v" id="trf_imbe_ago">--</td></tr>
        <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Vocoder (mbelib)</th></tr>
        <tr><th>PCM Produced</th><td class="v" id="voc_pcm">0</td></tr>
        <tr><th>Errors (>4 bit)</th><td class="v" id="voc_err">0</td></tr>
        <tr><th>Encrypted Skip</th><td class="v" id="voc_enc">0</td></tr>
      </tbody>
    </table>
  </div>
</div>

<!-- Call recordings. Ring buffer of recent calls, newest first.
     Each row has an inline <audio> control so the user can play
     back without leaving the page. -->
<div class="card" style="margin-top:14px">
  <h2>Recent Call Recordings <span id="rec_count" style="font-size:0.75em;color:var(--text-dim);margin-left:6px"></span></h2>
  <table style="width:100%">
    <thead>
      <tr>
        <th style="width:6em">Started</th>
        <th style="width:5em">TG</th>
        <th style="width:5em">Duration</th>
        <th style="width:6em">Size</th>
        <th>Playback</th>
      </tr>
    </thead>
    <tbody id="rec_tbody">
      <tr><td colspan="5" style="color:var(--text-dim)">No recordings yet.</td></tr>
    </tbody>
  </table>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
    Ring-buffered to /tmp/p25_recordings (tmpfs, lost on reboot).
    Oldest recording is evicted when the ring fills. Encrypted calls
    and calls shorter than 500 ms are not recorded.
  </p>
</div>

<!-- Phase 10-prep: TG Monitor (scanner mode). When any TG is
     checked the grant follower only retunes for those TGs. All
     unchecked = accept-all. List is populated from /api/grant_map
     (every TG we've ever seen a grant for on this site) so the
     user can build the watchlist from the real site roster. -->
<h2>TG Monitor <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">scanner-mode allow-list, hooked into grant pipeline via /api/monitor</span></h2>
<div class="card">
  <div style="display:flex;flex-wrap:wrap;align-items:center;gap:8px;margin-bottom:8px;font-size:0.85em">
    <span class="v" id="tgmon_status" style="color:var(--text-dim)">loading...</span>
    <button class="btn" style="padding:2px 10px" onclick="applyMonitorTgs()">Apply</button>
    <button class="btn" style="padding:2px 10px" onclick="clearMonitorTgs()">Clear all</button>
    <button class="btn" style="padding:2px 10px" onclick="refreshMonitorTgs()">Refresh roster</button>
    <label style="margin-left:10px"><input type="checkbox" id="tgmon_hide_enc" checked onchange="renderTgMonitor()"> Hide encrypted TGs</label>
  </div>
  <div id="tgmon_body" style="display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:6px;font-size:0.85em">
    <span style="color:var(--text-dim)">Waiting for grant_map + monitor state...</span>
  </div>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:8px">
    Empty allow-list = follower accepts every non-encrypted grant
    (default). Check one or more TGs + Apply to lock the follower
    to just those. Encryption filter runs independently and still
    blocks encrypted TGs even when they're on the list.
  </p>
</div>

<h2>Frequency Map</h2>
<div class="freq-map" id="freqmap"></div>

<div class="grid2">
  <div class="card">
    <h2>Active Grants</h2>
    <table>
      <thead><tr><th>Channel</th><th>Talkgroup</th><th>Source</th><th>Frequency</th><th>Age</th></tr></thead>
      <tbody id="grants_t"></tbody>
    </table>
  </div>
  <div class="card">
    <h2>Frequency Bands</h2>
    <table>
      <thead><tr><th>Band</th><th>Base (MHz)</th><th>Spacing</th><th>TX Offset</th><th>BW</th></tr></thead>
      <tbody id="bands_t"></tbody>
    </table>
  </div>
</div>

<h2>Live Activity</h2>
<div class="activity-filters" id="actFilters">
  <label class="af"><input type="checkbox" data-types="GRP_GRANT" checked><span class="af-swatch" style="background:var(--green)"></span>Grants</label>
  <label class="af"><input type="checkbox" data-types="GRANT_UPD,TEL_INT_GRANT_UPD" checked><span class="af-swatch" style="background:var(--accent)"></span>Grant Updates</label>
  <label class="af"><input type="checkbox" data-types="NET_STS,RFSS_STS" checked><span class="af-swatch" style="background:var(--orange)"></span>Network/RFSS</label>
  <label class="af"><input type="checkbox" data-types="IDEN_UP"><span class="af-swatch" style="background:var(--purple)"></span>Band IDs</label>
  <label class="af"><input type="checkbox" data-types="SCCB"><span class="af-swatch" style="background:var(--text-dim)"></span>SCCB</label>
  <label class="af"><input type="checkbox" data-types="SNDCP_ANN"><span class="af-swatch" style="background:var(--text-dim)"></span>SNDCP</label>
  <label class="af"><input type="checkbox" data-types="TDMA_SYNC"><span class="af-swatch" style="background:var(--text-dim)"></span>TDMA Sync</label>
  <label class="af"><input type="checkbox" data-types="UU_ANS_REQ" checked><span class="af-swatch" style="background:var(--text)"></span>UU Calls</label>
  <label class="af"><input type="checkbox" data-types="TRF_HDU,TRF_LDU1,TRF_LDU2,TRF_TDU,TRF_TDU_LC" checked><span class="af-swatch" style="background:#4db6ac"></span>Traffic CH</label>
</div>
<div class="card">
  <div id="activity"></div>
</div>

</div><!-- /#tab-radio -->

<!-- ═════════════════════ Logs tab ═════════════════════ -->
<div class="tab-pane" id="tab-logs">
<h2>Event Log <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">structured pipeline events (grants, traffic, imbe, vocoder)</span></h2>
<div class="log-toolbar">
  <label><input type="checkbox" id="logCatGrant"   checked> Grant</label>
  <label><input type="checkbox" id="logCatTraffic" checked> Traffic</label>
  <label><input type="checkbox" id="logCatImbe"    checked> IMBE</label>
  <label><input type="checkbox" id="logCatVocoder" checked> Vocoder</label>
  <label><input type="checkbox" id="logCatSystem"  checked> System</label>
  <label style="margin-left:8px"><input type="checkbox" id="logAutoscroll" checked> Auto-scroll</label>
  <button class="btn" onclick="logClear()">Clear View</button>
  <span class="log-status" id="logStatus">--</span>
</div>
<div class="log-viewer" id="logViewer"></div>
</div><!-- /#tab-logs -->

<!-- Aliases Modal -->
<div class="modal-overlay" id="aliasModal">
  <div class="modal">
    <h2>Talkgroup Aliases</h2>
    <p style="color:var(--text-dim);font-size:0.85em;margin:6px 0">JSON map: talkgroup ID (number) &rarr; display name</p>
    <textarea id="aliasText"></textarea>
    <div class="modal-btns">
      <button class="btn" onclick="closeAliases()">Cancel</button>
      <button class="btn btn-primary" onclick="saveAliases()">Save</button>
    </div>
  </div>
</div>

<script>
const $ = id => document.getElementById(id);
let aliases = {};

async function fetchJson(url) {
  try { return await (await fetch(url)).json(); } catch { return null; }
}

// Retune front-end via /api/reinit. Each of the three inputs
// (Control MHz / Center MHz / BW MHz) is independently optional.
// Unfilled fields preserve the current live value so a live
// iio_attr-set 8 MHz BW doesn't get silently reset to the 4 MHz
// boot default. Auto-LO: if the user sets Control alone and the
// target falls outside rx_lo ± BW/2, Center is filled automatically
// so the LO follows. Blank Control = restore boot control_freq.
async function retuneControl() {
  const ctrlStr   = $('bi_retune_mhz').value.trim();
  const centerStr = $('bi_center_mhz').value.trim();
  const bwStr     = $('bi_bw_mhz').value.trim();
  const status = $('bi_retune_status');
  const btn = $('bi_retune_btn');
  btn.disabled = true;
  status.textContent = 'retuning...';
  status.style.color = 'var(--text-dim)';
  try {
    // Current live front-end state. Both rf_bandwidth_hz and rx_lo_hz
    // come from /api/stats (readback of the AD9361 via libiio).
    const curStats = await fetchJson('/api/stats');
    const curBwHz = (curStats && curStats.rf_bandwidth_hz) || 0;
    const curLoHz = (curStats && curStats.rx_lo_hz)        || 0;
    const params = new URLSearchParams();

    // --- Control frequency ------------------------------------
    let ctrlHz = null;
    if (ctrlStr !== '') {
      const mhz = parseFloat(ctrlStr);
      if (!isFinite(mhz) || mhz < 100 || mhz > 6000) {
        status.textContent = 'bad Control MHz (100-6000 expected)';
        status.style.color = 'var(--red)';
        btn.disabled = false;
        return;
      }
      ctrlHz = Math.round(mhz * 1e6);
      params.set('control_freq', String(ctrlHz));
    }

    // --- rf_bandwidth -----------------------------------------
    // If user gave an explicit value, use it; otherwise preserve
    // current live BW so we don't fall back to the 4 MHz boot default.
    let bwHz = curBwHz;
    if (bwStr !== '') {
      const mhz = parseFloat(bwStr);
      if (!isFinite(mhz) || mhz < 0.2 || mhz > 56) {
        status.textContent = 'bad BW MHz (0.2-56 expected)';
        status.style.color = 'var(--red)';
        btn.disabled = false;
        return;
      }
      bwHz = Math.round(mhz * 1e6);
    }
    if (bwHz > 0) params.set('rf_bandwidth', String(bwHz));

    // --- Center / rx_lo ---------------------------------------
    // Explicit center wins. Otherwise auto-nudge LO if control_freq
    // would fall outside rx_lo ± bwHz/2. The AD9361 DDC chain can
    // still decode slightly outside the analog filter but SNR
    // degrades fast — keep the channel inside the filter skirt.
    let loHz = null;
    if (centerStr !== '') {
      const mhz = parseFloat(centerStr);
      if (!isFinite(mhz) || mhz < 70 || mhz > 6000) {
        status.textContent = 'bad Center MHz (70-6000 expected)';
        status.style.color = 'var(--red)';
        btn.disabled = false;
        return;
      }
      loHz = Math.round(mhz * 1e6);
    } else if (ctrlHz != null && curLoHz > 0 && bwHz > 0) {
      const halfBw = bwHz / 2;
      if (Math.abs(ctrlHz - curLoHz) > halfBw) {
        // Park LO on the requested control freq so the DDC sits
        // inside the filter passband.
        loHz = ctrlHz;
      }
    }
    if (loHz != null) params.set('rx_lo', String(loHz));

    const url = '/api/reinit' + (params.toString() ? '?' + params.toString() : '');
    const res = await fetchJson(url);
    if (res && res.ok) {
      const parts = [];
      if (ctrlHz != null)   parts.push(`ctrl=${(ctrlHz/1e6).toFixed(4)}`);
      if (loHz != null)     parts.push(`lo=${(loHz/1e6).toFixed(3)}`);
      if (bwStr !== '')     parts.push(`bw=${(bwHz/1e6).toFixed(1)}`);
      const desc = parts.length ? parts.join(' ') : 'boot defaults';
      status.textContent = `tuned: ${desc} — waiting for reacquire...`;
      status.style.color = 'var(--green)';
      // Force immediate refreshes so the user sees the new NAC
      // appear as soon as the decoder locks.
      setTimeout(refresh, 500);
      setTimeout(refresh, 2000);
      setTimeout(refresh, 5000);
    } else {
      const errs = (res && res.errors && res.errors.join('; ')) || 'reinit failed';
      status.textContent = errs;
      status.style.color = 'var(--red)';
    }
  } catch (e) {
    status.textContent = 'fetch error: ' + e;
    status.style.color = 'var(--red)';
  } finally {
    btn.disabled = false;
  }
}

// Phase 10-prep: tab-visibility helper. Used to gate polling so
// endpoints that only feed one tab don't get fetched when that
// tab is hidden. Before this, refresh() fetched 10 endpoints every
// 2 s unconditionally on the Z7020 ARM, which starved /ws/audio
// chunk delivery and caused the live-audio stutter the user heard
// mid-call. Now the Debug-tab-only fetches skip when Debug isn't
// active, and vice versa.
function activeTab() {
  const btn = document.querySelector('#tabNav button.active');
  return btn ? btn.dataset.tab : null;
}

async function refresh() {
  const tab = activeTab();

  // /api/system drives the header build tag + NAC/WACN identity
  // card on both Radio and Debug tabs, so fetch it regardless of
  // active tab. It's small (a few hundred bytes) and the
  // header is visible on every tab.
  const sys = await fetchJson('/api/system');
  if (sys) {
    $('nac').textContent = sys.nac || '--';
    $('wacn').textContent = sys.wacn || '--';
    $('sys').textContent = sys.system_id || '--';
    $('rfss').textContent = (sys.rfss_id != null ? `${sys.rfss_id} / ${sys.site_id}` : '--');
    $('cc').textContent = sys.control_channel || '--';
    if (sys.build) $('build_tag').textContent = 'build: ' + sys.build;
    // Stash the phase label for refreshModulation() to use when it
    // builds the "System type" combined readout (modulation · phase).
    window._LAST_SYS_PHASE = sys.phase || '';
  }

  // ── Debug-tab-only block ────────────────────────────────────────
  // Everything between here and the matching close-brace only
  // feeds widgets on the Debug tab. Skip entirely when the tab
  // isn't visible — these endpoints (decoder_compare, hdl_lsm,
  // irq_stats, control_lsm_dibit_dump) collectively account for
  // ~half of refresh()'s per-cycle cost on the Zynq-7020.
  if (tab === 'debug') {

  // ── Phase 6F.2: Decoder Comparison Matrix ──
  const cmp = await fetchJson('/api/decoder_compare');
  if (cmp) {
    const fmtN = v => (v == null) ? '--' : (typeof v === 'number' ? v.toLocaleString() : v);
    const fmtPct = v => (v == null) ? '--' : v.toFixed(1) + '%';
    // Phase 9: 3-column matrix (was 5). Columns are:
    //   1. PS C4FM  — dormant fallback, only used on C4FM sites
    //   2. PS LSM   — software framer on PL HDL LSM dibits (production)
    //   3. PL HDL   — FPGA LSM chain's own runtime stats from the heartbeat
    //
    // PL-derived aliases for metrics that the HDL doesn't expose
    // verbatim but can be computed from the cumulative event
    // counters in HdlLsmRuntime:
    //   - PL sync hits        = total_nid_events (every HDL hard-sync
    //                            hit fires a NID BCH sweep 1:1)
    //   - PL NID attempts     = total_nid_events (sync hit == attempt)
    //   - PL NID BCH failures = total - valid (valid := dist <= 11)
    //   - PL NID decoded OK   = valid_nid_events
    //   - PL total dibits     = cmp.ps_lsm.total_dibits (the PS framer
    //                            is a pass-through dibit counter on
    //                            the PL LSM dibit DMA ring; same
    //                            number, different source of truth)
    const pl_total_nids = cmp.pl_hdl.total_nids || 0;
    const pl_valid_nids = cmp.pl_hdl.valid_nids || 0;
    const pl_nid_fail = pl_total_nids - pl_valid_nids;
    const pl_total_dibits = cmp.ps_lsm.total_dibits; // pass-through
    // 2026-04-16: PS C4FM column removed. Matrix is now 2-column
    // (PS LSM framer | PL HDL LSM gateware) since the HDL LSM
    // chain decodes both C4FM and LSM sites.
    const rows = [
      ['NAC (winner)', cmp.ps_lsm.system_nac, cmp.pl_hdl.winner_nac],
      ['Messages decoded', fmtN(cmp.ps_lsm.messages), '(PS only)'],
      ['Total NIDs', '--', fmtN(pl_total_nids)],
      ['Valid NIDs', '--', fmtN(pl_valid_nids) + ' (' + fmtPct(cmp.pl_hdl.valid_pct) + ')'],
      ['Sync hits (frame sync correlator)', fmtN(cmp.ps_lsm.sync_hits), fmtN(pl_total_nids)],
      ['Sync near-misses', fmtN(cmp.ps_lsm.sync_near), '(HDL: hit-only)'],
      ['Sync best Hamming distance', fmtN(cmp.ps_lsm.sync_best_dist), fmtN(cmp.pl_hdl.sync_distance)],
      ['Total dibits processed', fmtN(cmp.ps_lsm.total_dibits), fmtN(pl_total_dibits) + ' (= PS)'],
      ['Active grants', fmtN(cmp.ps_lsm.active_grants), '(PS only)'],
      ['Frequency bands known', fmtN(cmp.ps_lsm.bands_known), '(PS only)'],
      ['Drop count (PL only)', '--', fmtN(cmp.pl_hdl.drop_count)],
      ['Live PLL register', '--', fmtN(cmp.pl_hdl.pll_dbg)],
      ['Live sample-point register', '--', fmtN(cmp.pl_hdl.sp_dbg)],
      ['Overflow events', '--', 'dibit:' + fmtN(cmp.pl_hdl.dibit_overflow_ticks) + ' iq:' + fmtN(cmp.pl_hdl.iq_overflow_ticks)],
      ['── pipeline ──', '', ''],
      ['NID attempts (sync hit)', fmtN(cmp.ps_lsm.nid_attempts), fmtN(pl_total_nids)],
      ['NID BCH decode failures', fmtN(cmp.ps_lsm.nid_decode_failures), fmtN(pl_nid_fail)],
      ['NID invalid DUID after BCH', fmtN(cmp.ps_lsm.nid_invalid_duid), '(HDL: always valid)'],
      ['NID decoded OK (any DUID)', fmtN(cmp.ps_lsm.nid_decoded_ok), fmtN(pl_valid_nids)],
      ['NID decoded OK (TSDU only)', fmtN(cmp.ps_lsm.nid_decoded_tsdu), '(PS only)'],
      ['TSDU attempts', fmtN(cmp.ps_lsm.tsdu_attempts), '(PS framer)'],
      ['TSBK block attempts', fmtN(cmp.ps_lsm.tsbk_block_attempts), '(PS framer)'],
      ['TSBK trellis failures', fmtN(cmp.ps_lsm.tsbk_trellis_failures), '(PS framer)'],
      ['TSBK CRC failures', fmtN(cmp.ps_lsm.tsbk_crc_failures), '(PS framer)'],
      ['TSBK CRC OK', fmtN(cmp.ps_lsm.tsbk_crc_ok), '(PS framer)'],
      ['  - via plain CRC convention', fmtN(cmp.ps_lsm.tsbk_crc_ok_plain), '(PS framer)'],
      ['  - via xored 0xFFFF convention', fmtN(cmp.ps_lsm.tsbk_crc_ok_xored), '(PS framer)'],
      ['TSBK unknown opcode', fmtN(cmp.ps_lsm.tsbk_unknown_opcode), '(PS framer)'],
    ];
    diffList($('cmp_body'), r => r[0], rows,
      () => {
        const tr = document.createElement('tr');
        tr.appendChild(document.createElement('th'));
        const t1 = document.createElement('td'); t1.className = 'v'; tr.appendChild(t1);
        const t2 = document.createElement('td'); t2.className = 'v'; tr.appendChild(t2);
        return tr;
      },
      (tr, r) => {
        if (tr.children[0].textContent !== r[0]) tr.children[0].textContent = r[0];
        const v1 = String(r[1]), v2 = String(r[2]);
        if (tr.children[1].textContent !== v1) tr.children[1].textContent = v1;
        if (tr.children[2].textContent !== v2) tr.children[2].textContent = v2;
      });
  }

  // ── Phase 6F.2: PL HDL LSM Chain Detail ──
  const hdl = await fetchJson('/api/hdl_lsm');
  if (hdl) {
    const alive = hdl.running && hdl.last_tick_ms_ago != null && hdl.last_tick_ms_ago < 1000;
    if (!hdl.running) {
      $('hdl_status').textContent = 'NOT STARTED';
      $('hdl_status').style.color = 'var(--red)';
    } else if (alive) {
      $('hdl_status').textContent = 'ALIVE (' + hdl.uptime_secs + 's)';
      $('hdl_status').style.color = 'var(--green)';
    } else {
      $('hdl_status').textContent = 'STALLED';
      $('hdl_status').style.color = 'var(--red)';
    }
    $('hdl_nid_valid').textContent = hdl.cumulative.valid_nid_events.toLocaleString();
    $('hdl_nid_total').textContent = hdl.cumulative.total_nid_events.toLocaleString();
    $('hdl_nid_pct').textContent = hdl.cumulative.valid_pct.toFixed(1) + '%';
    $('hdl_last_nac').textContent = hdl.live.last_nac;
    $('hdl_last_duid').textContent = '0x' + hdl.live.last_duid.toString(16).toUpperCase();
    $('hdl_drop').textContent = hdl.live.last_drop_count;
    $('hdl_pll').textContent = hdl.live.pll_dbg;
    $('hdl_sp').textContent = hdl.live.sp_dbg;
    $('hdl_sd_now').textContent = hdl.live.sync_distance;
    $('hdl_sd_best').textContent = hdl.last_window.sync_dist_best === 99 ? '--' : hdl.last_window.sync_dist_best;
    $('hdl_bch').textContent = hdl.live.bch_busy ? 'YES' : 'no';
    $('hdl_inwin').textContent = hdl.live.in_nid_window ? 'YES' : 'no';
    $('hdl_w_pll').textContent = hdl.last_window.pll_min + ' / ' + hdl.last_window.pll_max;
    $('hdl_w_sp').textContent = hdl.last_window.sp_min + ' / ' + hdl.last_window.sp_max;
    $('hdl_w_nids').textContent = hdl.last_window.valid_count + ' / ' + hdl.last_window.event_count;
    $('hdl_w_iq').textContent = hdl.last_window.iq_kbps + ' KB/s, ' + hdl.last_window.iq_buf_rolls + ' rolls';
    $('hdl_ovf').textContent = hdl.cumulative.dibit_overflow_ticks + ' / ' + hdl.cumulative.iq_overflow_ticks;

    if (hdl.nid_ring && hdl.nid_ring.length) {
      const ring = hdl.nid_ring.slice().reverse();
      diffList($('nid_ring_body'), e => e.seq, ring,
        () => {
          const tr = document.createElement('tr');
          for (let i = 0; i < 10; i++) {
            const td = document.createElement('td');
            td.className = 'v';
            tr.appendChild(td);
          }
          return tr;
        },
        (tr, e) => {
          const c = tr.children;
          c[0].textContent = e.seq;
          c[1].textContent = e.t_ms_since_boot;
          c[2].textContent = '0x' + e.nac.toString(16).toUpperCase().padStart(3, '0');
          c[3].textContent = e.duid;
          c[4].textContent = e.valid ? '\u2713' : '\u2717';
          c[4].style.color = e.valid ? 'var(--green)' : 'var(--red)';
          c[5].textContent = e.n_errors;
          c[6].textContent = e.sync_distance;
          c[7].textContent = e.drop_count;
          c[8].textContent = e.pll_dbg;
          c[9].textContent = e.sp_dbg;
        });
    }
  }

  // ── Phase 6F.2: IRQ Source Counters ──
  const irq = await fetchJson('/api/irq_stats');
  if (irq) {
    const fmtRate = r => (r < 1 ? r.toFixed(2) : Math.round(r).toLocaleString()) + '/s';
    $('irq_total').textContent = irq.total.toLocaleString();
    $('irq_total_rate').textContent = fmtRate(irq.rate_per_sec.total);
    $('irq_dibit').textContent = irq.dibit.toLocaleString();
    $('irq_dibit_rate').textContent = fmtRate(irq.rate_per_sec.dibit);
    $('irq_traffic').textContent = irq.traffic.toLocaleString();
    $('irq_traffic_rate').textContent = fmtRate(irq.rate_per_sec.traffic);
    $('irq_iq').textContent = irq.iq.toLocaleString();
    $('irq_iq_rate').textContent = fmtRate(irq.rate_per_sec.iq);
    $('irq_lsm').textContent = irq.lsm_dibit.toLocaleString();
    $('irq_lsm_rate').textContent = fmtRate(irq.rate_per_sec.lsm_dibit);
    $('irq_last').textContent = irq.last_at_ms_ago != null ? irq.last_at_ms_ago : '--';
    $('irq_uptime').textContent = irq.uptime_secs + 's';
  }

  } // end Debug-tab-only block (cmp/hdl/irq)

  // /api/stats feeds Board Info on Radio (bi_*) AND the small
  // Decode Stats card on Debug (msgs/grants_n/bands_n/dibits/
  // overflow) AND the header dot status. Fetch unconditionally;
  // the per-field updates below are cheap even if the target
  // card is hidden.
  const stats = await fetchJson('/api/stats');
  if (stats) {
    $('msgs').textContent = stats.recent_messages.toLocaleString();
    $('grants_n').textContent = stats.active_grants;
    $('bands_n').textContent = stats.bands_known;
    $('dibits').textContent = stats.dibit_count.toLocaleString();
    $('overflow').textContent = stats.overflow ? 'YES' : 'No';
    $('overflow').style.color = stats.overflow ? 'var(--red)' : '';
    const d = $('dot'), s = $('status');
    if (stats.system_acquired) { d.classList.add('active'); s.textContent = 'Tracking'; }
    else { d.classList.remove('active'); s.textContent = 'Searching'; }

    // ── Board Info panel (Phase 10) ──
    // sys_* values come from the /api/system fetch above; everything
    // else lives in /api/stats. All fields are Option<...> server-side,
    // so guard against missing/null before formatting.
    const fmtHz = v => (v == null) ? '--' : (v / 1e6).toFixed(4) + ' MHz';
    const fmtKhz = v => (v == null) ? '--' : (v / 1e3).toFixed(1) + ' kHz';
    const fmtMsps = v => (v == null) ? '--' : (v / 1e6).toFixed(3) + ' MSPS';
    const fmtDb = v => (v == null) ? '--' : v.toFixed(1) + ' dB';
    const fmtUptime = s => {
      if (s == null) return '--';
      const d = Math.floor(s / 86400);
      const h = Math.floor((s % 86400) / 3600);
      const m = Math.floor((s % 3600) / 60);
      const sec = s % 60;
      if (d > 0) return `${d}d ${h}h ${m}m`;
      if (h > 0) return `${h}h ${m}m ${sec}s`;
      if (m > 0) return `${m}m ${sec}s`;
      return `${sec}s`;
    };
    $('bi_build').textContent = (sys && sys.build) || '--';
    $('bi_uptime').textContent = fmtUptime(stats.uptime_secs);
    $('bi_clock').textContent = stats.wall_clock || '--';
    if (sys && (sys.nac || sys.wacn)) {
      $('bi_nac').textContent = (sys.nac || '--') + ' / ' + (sys.wacn || '--');
    } else {
      $('bi_nac').textContent = '--';
    }
    if (sys && (sys.rfss_id != null || sys.site_id != null)) {
      $('bi_rfss').textContent = (sys.rfss_id != null ? sys.rfss_id : '--')
        + ' / ' + (sys.site_id != null ? sys.site_id : '--');
    } else {
      $('bi_rfss').textContent = '--';
    }
    $('bi_rx_lo').textContent = fmtHz(stats.rx_lo_hz);
    $('bi_rf_bw').textContent = fmtHz(stats.rf_bandwidth_hz);
    const gainTxt = (stats.rx_gain_db != null ? fmtDb(stats.rx_gain_db) : '--')
      + ' / ' + (stats.gain_control_mode || '--');
    $('bi_gain').textContent = gainTxt;
    $('bi_rssi').textContent = fmtDb(stats.rx_rssi_db);
    $('bi_sr').textContent = fmtMsps(stats.sampling_frequency_hz);
    $('bi_ddc').textContent = stats.ddc_decimation || '--';
    $('bi_ddc_off').textContent = fmtKhz(stats.ddc_control_offset_hz);
    $('bi_ddc_out').textContent = (stats.ddc_output_rate_hz == null) ? '--' :
      (stats.ddc_output_rate_hz / 1e3).toFixed(3) + ' kHz';
    $('bi_ws_clients').textContent = (stats.audio_ws_clients == null) ?
      '--' : stats.audio_ws_clients;
    const lag = stats.audio_ws_lag_total || 0;
    $('bi_ws_lag').textContent = lag.toLocaleString();
    $('bi_ws_lag').style.color = lag > 0 ? 'var(--orange)' : '';
  }

  // Phase 9 retirement: the `/api/lsm` fetch + "LSM Pipeline"
  // card updater went here. Retired along with the Phase 6D
  // software pipeline; PL HDL stats come from `/api/hdl_lsm`
  // above.

  // Helper to populate one of the dibit-stream cards (C4FM or LSM).
  const renderDibitDump = (dump, ids) => {
    if (!dump) return;
    const fmt = (n, p) => `${n.toLocaleString()} (${p.toFixed(1)}%)`;
    $(ids.total).textContent = dump.total_dibits.toLocaleString();
    $(ids.v0).textContent = fmt(dump.histogram['0'], dump.histogram['0_pct']);
    $(ids.v1).textContent = fmt(dump.histogram['1'], dump.histogram['1_pct']);
    $(ids.v2).textContent = fmt(dump.histogram['2'], dump.histogram['2_pct']);
    $(ids.v3).textContent = fmt(dump.histogram['3'], dump.histogram['3_pct']);
    $(ids.io).textContent = dump.histogram.inner_pct.toFixed(1) + '% / ' + dump.histogram.outer_pct.toFixed(1) + '%';
    $(ids.hits).textContent = dump.sync.hits.toLocaleString();
    $(ids.near).textContent = dump.sync.near_misses.toLocaleString();
    const bd = dump.sync.best_distance;
    $(ids.best).textContent = (bd >= 4294967000) ? '--' : bd;
    $(ids.rd_total).textContent = dump.raw_duid.total.toLocaleString();
    if (dump.raw_duid.total > 0) {
      $(ids.rd_7).textContent = dump.raw_duid.pct_7_tsdu.toFixed(1) + '%';
      $(ids.rd_5).textContent = dump.raw_duid.pct_5_ldu1.toFixed(1) + '%';
      $(ids.rd_a).textContent = dump.raw_duid.pct_a_ldu2.toFixed(1) + '%';
      $(ids.rd_0).textContent = dump.raw_duid.pct_0_hdu.toFixed(1) + '%';
    }
  };

  // C4FM HDL dibit dump card was removed from the Debug tab on
  // 2026-04-16 — the HDL LSM chain decodes both C4FM and LSM so
  // the PS C4FM pipeline is dormant everywhere. Only the LSM
  // dibit histogram is still interesting.
  if (tab === 'debug') {
    const lsmIds = {total:'ldh_total', v0:'ldh_0', v1:'ldh_1', v2:'ldh_2', v3:'ldh_3', io:'ldh_io',
      hits:'lsy_hits', near:'lsy_near', best:'lsy_best',
      rd_total:'lrd_total', rd_7:'lrd_7', rd_5:'lrd_5', rd_a:'lrd_a', rd_0:'lrd_0'};
    renderDibitDump(await fetchJson('/api/control_lsm_dibit_dump'), lsmIds);
  }

  // ── Radio-tab-only block ────────────────────────────────────────
  // /api/stats powers the Board Info card; /api/traffic powers the
  // grant follower + IMBE/vocoder card; /api/grants + /api/bands
  // feed Active Grants + Frequency Bands. None of these are
  // visible outside Radio.
  if (tab === 'radio') {

  // ── Phase 7D: Traffic Channel + Vocoder panel ──
  const trf = await fetchJson('/api/traffic');
  if (trf) {
    $('trf_phase').textContent = 'Phase ' + (trf.phase || '?');
    $('trf_state').textContent = trf.state || '--';
    if (trf.current_talkgroup != null) {
      const encBadge = trf.current_call_encrypted ? ' [ENC]' : '';
      $('trf_tg').textContent = trf.current_talkgroup + encBadge;
      $('trf_tg').style.color = trf.current_call_encrypted ? 'var(--red)' : '';
    } else {
      $('trf_tg').textContent = '(idle)';
      $('trf_tg').style.color = 'var(--text-dim)';
    }
    if (trf.current_frequency_hz != null) {
      $('trf_freq').textContent = (trf.current_frequency_hz / 1e6).toFixed(4) + ' MHz';
    } else {
      $('trf_freq').textContent = '--';
    }
    $('trf_enc').textContent = trf.current_call_encrypted == null ? '--' :
      (trf.current_call_encrypted ? 'YES' : 'No');
    $('trf_enc').style.color = trf.current_call_encrypted ? 'var(--red)' : 'var(--green)';
    $('trf_grants').textContent = (trf.grants_seen || 0).toLocaleString();
    $('trf_retunes').textContent = trf.retunes || 0;
    $('trf_rej_enc').textContent = (trf.grants_rejected_encrypted || 0).toLocaleString();
    $('trf_duid').textContent = (trf.last_duid_label || '--') + ' (' + (trf.last_duid_hex || '--') + ')';
    if (trf.last_retune_secs_ago != null) {
      $('trf_last_retune').textContent = Math.round(trf.last_retune_secs_ago) + 's ago';
    } else {
      $('trf_last_retune').textContent = '--';
    }
    const im = trf.imbe || {};
    $('trf_hdu').textContent = (im.hdu_count || 0).toLocaleString();
    $('trf_ldu1').textContent = (im.ldu1_count || 0).toLocaleString();
    $('trf_ldu2').textContent = (im.ldu2_count || 0).toLocaleString();
    $('trf_tdu').textContent = (im.tdu_count || 0).toLocaleString() + ' / ' +
      (im.tdu_lc_count || 0).toLocaleString();
    $('trf_imbe').textContent = (im.imbe_frames_extracted || 0).toLocaleString();
    const ldu_total = (im.ldu1_count || 0) + (im.ldu2_count || 0);
    $('trf_imbe_exp').textContent = (ldu_total * 9).toLocaleString();
    $('trf_imbe_drop').textContent = (im.imbe_frames_dropped || 0).toLocaleString();
    $('trf_imbe_drop').style.color = (im.imbe_frames_dropped || 0) > 0 ? 'var(--red)' : '';
    $('trf_imbe_drop_idle').textContent = (im.imbe_frames_dropped_idle || 0).toLocaleString();
    $('trf_imbe_drop_idle').style.color = (im.imbe_frames_dropped_idle || 0) > 0 ? 'var(--orange)' : '';
    if (im.last_imbe_secs_ago != null) {
      $('trf_imbe_ago').textContent = Math.round(im.last_imbe_secs_ago) + 's ago';
    } else {
      $('trf_imbe_ago').textContent = 'never';
    }
    $('voc_pcm').textContent = (im.vocoder_pcm_produced || 0).toLocaleString();
    $('voc_err').textContent = (im.vocoder_errors || 0).toLocaleString();
    $('voc_enc').textContent = (im.vocoder_frames_encrypted || 0).toLocaleString();
    // Status indicator
    const pcm = im.vocoder_pcm_produced || 0;
    if (pcm > 0) {
      $('voc_status').textContent = 'ACTIVE (' + pcm.toLocaleString() + ' samples)';
      $('voc_status').style.color = 'var(--green)';
    } else {
      $('voc_status').textContent = 'WAITING';
      $('voc_status').style.color = 'var(--text-dim)';
    }
  }

  const grants = await fetchJson('/api/grants');
  if (grants) {
    $('grants_t').innerHTML = grants.map(g => {
      // Red [ENC] badge when the latest TSBK flags it encrypted.
      // Orange [ENC-HIST] when the flag isn't set on this specific
      // TSBK but the TG has ever been seen encrypted (service-options
      // are often absent on Motorola/Harris sites so the history is
      // the reliable truth).
      let badge = '';
      if (g.encrypted) {
        badge = ' <span style="color:var(--red);font-weight:600">[ENC]</span>';
      } else if (g.in_encrypted_history) {
        badge = ' <span style="color:var(--orange);font-weight:600">[ENC-HIST]</span>';
      }
      const rowStyle = (g.encrypted || g.in_encrypted_history)
        ? ' style="opacity:0.6"' : '';
      return `<tr${rowStyle}><td>${g.channel}</td>` +
        `<td class="tg">${g.talkgroup}${badge}${g.talkgroup_alias ? ' <span class="alias">' + g.talkgroup_alias + '</span>' : ''}</td>` +
        `<td>${g.source ?? ''}</td>` +
        `<td class="freq">${g.frequency_mhz ? g.frequency_mhz.toFixed(4) : ''}</td>` +
        `<td>${g.age_secs}s</td></tr>`;
    }).join('') || '<tr><td colspan="5" style="color:var(--text-dim)">None</td></tr>';
    renderFreqMap(grants);
  }

  const bands = await fetchJson('/api/bands');
  if (bands) {
    $('bands_t').innerHTML = bands.map(b =>
      `<tr><td>${b.identifier}</td>` +
      `<td class="freq">${b.base_frequency_mhz.toFixed(5)}</td>` +
      `<td>${b.channel_spacing_khz} kHz</td>` +
      `<td>${b.transmit_offset_mhz} MHz</td>` +
      `<td>${b.bandwidth_khz} kHz</td></tr>`
    ).join('') || '<tr><td colspan="5" style="color:var(--text-dim)">None</td></tr>';
  }

  } // end Radio-tab-only block
}

// Frequency map rendering
const LCN_DATA = [
  {lcn:1,freq:855.2375},{lcn:2,freq:856.4375},{lcn:3,freq:857.2125},
  {lcn:4,freq:857.4375},{lcn:5,freq:857.9875},{lcn:6,freq:858.4375},
  {lcn:7,freq:858.4625},{lcn:8,freq:858.9875},{lcn:9,freq:859.4375},
  {lcn:10,freq:860.4375},{lcn:11,freq:860.9625}
];

function renderFreqMap(grants) {
  const map = $('freqmap');
  if (!LCN_DATA.length) return;
  const minF = LCN_DATA[0].freq - 0.5;
  const maxF = LCN_DATA[LCN_DATA.length-1].freq + 0.5;
  const range = maxF - minF;
  const activeFreqs = new Set((grants||[]).map(g => g.frequency_mhz ? g.frequency_mhz.toFixed(4) : null).filter(Boolean));

  let html = '';
  for (const lcn of LCN_DATA) {
    const pct = ((lcn.freq - minF) / range * 100).toFixed(1);
    const isCC = lcn.lcn === 11;
    const isActive = activeFreqs.has(lcn.freq.toFixed(4));
    const cls = isCC ? 'cc' : (isActive ? 'active' : '');
    html += `<div class="freq-marker ${cls}" style="left:${pct}%"></div>`;
    html += `<div class="freq-label" style="left:${pct}%">${lcn.freq.toFixed(2)}</div>`;
    html += `<div class="freq-lcn" style="left:${pct}%">${lcn.lcn}${isCC?' CC':''}${isActive?' ★':''}</div>`;
  }
  map.innerHTML = html;
}

// Activity filter state
function getActiveFilters() {
  const active = new Set();
  document.querySelectorAll('#actFilters input[type=checkbox]').forEach(cb => {
    if (cb.checked) {
      cb.dataset.types.split(',').forEach(t => active.add(t));
    }
  });
  return active;
}

// WebSocket
function connectWs() {
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const ws = new WebSocket(`${proto}//${location.host}/ws/events`);
  ws.onmessage = e => {
    try {
      const evt = JSON.parse(e.data);
      const filters = getActiveFilters();
      if (!filters.has(evt.event_type)) return;
      const el = document.createElement('div');
      el.className = 'evt';
      el.dataset.type = evt.event_type;
      const alias = evt.talkgroup_alias ? ` <span class="alias">${evt.talkgroup_alias}</span>` : '';
      el.innerHTML =
        `<span class="evt-time">${evt.timestamp}</span>` +
        `<span class="evt-type ${evt.event_type}">${evt.event_type}</span>` +
        `<span class="evt-detail">${evt.summary}${alias}</span>`;
      const act = $('activity');
      // column-reverse: first child is at the bottom (newest visually at top)
      act.prepend(el);
      while (act.children.length > 300) act.lastChild.remove();
    } catch {}
    refresh();
  };
  ws.onclose = () => setTimeout(connectWs, 3000);
  ws.onerror = () => ws.close();
}

// When a filter checkbox changes, hide/show existing matching events
document.querySelectorAll('#actFilters input[type=checkbox]').forEach(cb => {
  cb.addEventListener('change', () => {
    const types = cb.dataset.types.split(',');
    document.querySelectorAll('#activity .evt').forEach(el => {
      if (types.includes(el.dataset.type)) {
        el.style.display = cb.checked ? '' : 'none';
      }
    });
  });
});

// Theme
function toggleTheme() {
  const body = document.body;
  const isLight = body.getAttribute('data-theme') === 'light';
  body.setAttribute('data-theme', isLight ? '' : 'light');
  $('themeBtn').textContent = isLight ? '\u{1f319}' : '\u2600\ufe0f';
  localStorage.setItem('theme', isLight ? 'dark' : 'light');
}
(function() {
  if (localStorage.getItem('theme') === 'light') {
    document.body.setAttribute('data-theme', 'light');
    $('themeBtn').textContent = '\u2600\ufe0f';
  }
})();

// Aliases
async function loadAliases() {
  aliases = await fetchJson('/api/aliases') || {};
}
function showAliases() {
  $('aliasText').value = JSON.stringify(aliases, null, 2);
  $('aliasModal').classList.add('show');
}
function closeAliases() { $('aliasModal').classList.remove('show'); }
async function saveAliases() {
  try {
    const parsed = JSON.parse($('aliasText').value);
    await fetch('/api/aliases', {
      method: 'PUT',
      headers: {'Content-Type': 'application/json'},
      body: JSON.stringify(parsed)
    });
    aliases = parsed;
    closeAliases();
    refresh();
  } catch (e) { alert('Invalid JSON: ' + e.message); }
}

// ── Phase 10: AudioWorklet live audio player ──
//
// Replaces the Phase 7E per-chunk BufferSource scheduler, which produced
// robotic playback even though the server-side IMBE frames were clean
// (verified 2026-04-15 via /api/audio_test WAV: same bits, different
// decoder instance, playback is clean). Root cause: each 20 ms incoming
// chunk was wrapped in its own `AudioBuffer` + `BufferSource` and
// scheduled independently onto the `AudioContext` timeline, so every
// chunk got its own transient 8 kHz -> 48 kHz resample with no state
// carried across chunk boundaries. SDRTrunk's Java pipeline doesn't
// have this problem because JavaSound provides a blocking
// `SourceDataLine.write()` into a single 8 kHz ring buffer; the
// AudioWorklet pattern is the Web Audio equivalent.
//
// Architecture (matches SDRTrunk AudioChannel.java conceptually):
//   1. One `AudioContext` + one `AudioWorkletNode` for the lifetime of
//      the session.
//   2. Worklet owns a `Float32Array` ring buffer big enough for a
//      couple of seconds of 8 kHz audio.
//   3. Main thread reads incoming WS binary frames, converts i16 -> f32,
//      and `postMessage`s them to the worklet.
//   4. Worklet's `process()` callback runs at the `AudioContext`'s
//      native rate (48 kHz on most browsers) and emits samples via
//      linear interpolation from the 8 kHz ring. One continuous
//      resample, no per-chunk state.
//   5. On an empty ring, the worklet emits silence and increments an
//      `underruns` counter — matches SDRTrunk's "return null/silence"
//      behaviour when AudioBuffer has < 160 samples.
//   6. Status line polls the worklet for `{available, underruns,
//      totalOut}` every 250 ms over `port.postMessage`.
//
// The worklet module itself is defined as a string constant and loaded
// via a `Blob` URL so there's no separate /audio_worklet.js route.

const AUDIO_WORKLET_CODE = `
class P25AudioProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    // 2 s of 8 kHz mono = 16384 samples. Bigger than any realistic
    // broadcast-side burst (9 IMBE frames = 180 ms) plus a full
    // broadcast::channel(256) worth of backlog, so we can absorb a
    // server hiccup and still sound continuous.
    this.RING = 16384;
    this.ring = new Float32Array(this.RING);
    this.write = 0;
    this.read = 0;
    this.available = 0;
    this.underruns = 0;
    this.totalOut = 0;
    this.totalIn = 0;
    this.SRC_RATE = 8000;
    this.ratio = this.SRC_RATE / sampleRate;
    this.readFrac = 0;
    // Prefill target: the vocoder task delivers 9 AudioChunks in a
    // ~1 ms burst once per LDU, then waits ~180 ms for the next LDU
    // to finish over-the-air. So the incoming stream is bursty with
    // 180 ms silent gaps between bursts, and the ring naturally
    // oscillates between 0 and 180 ms of buffered audio. Prefilling
    // to only 200 ms puts us right at the edge of that oscillation;
    // any jitter immediately underruns. 4320 samples = 540 ms = 3
    // LDUs of headroom, which absorbs both LDU-burst jitter and the
    // occasional main-thread stall from the dashboard's 2 s refresh
    // loop. Costs ~340 ms of extra initial latency, which is still
    // well under the 1-2 s "starts late" threshold the operator
    // would notice relative to visible dashboard state.
    this.PREFILL = 4320;
    this.priming = true;
    // Consecutive per-sample underruns before we re-enter priming.
    // At a typical 48 kHz AudioContext, 4800 samples = 100 ms of
    // continuous silence. If we've produced 100 ms of underrun in
    // one stretch the network has genuinely stalled and we should
    // hold silence until the ring is healthy again, rather than
    // stuttering out 20 ms bursts each time a single LDU arrives.
    // The original logic absorbed individual-sample underruns
    // silently (correct) but never re-primed on sustained gaps,
    // producing the "sounds like it's cutting out 4 times a
    // second" effect when the dashboard's debug-tab pollers were
    // starving /ws/audio chunk delivery.
    this.UNDERRUN_REPRIME = 4800;
    this.consecUnderruns = 0;
    this.port.onmessage = (ev) => {
      const m = ev.data;
      if (m.type === 'pcm') {
        const d = m.data;
        for (let i = 0; i < d.length; i++) {
          this.ring[this.write] = d[i];
          this.write = (this.write + 1) % this.RING;
          if (this.available < this.RING) {
            this.available++;
          } else {
            // Ring full — drop oldest (this should never happen on a
            // healthy LAN; it means the vocoder produced faster than
            // sampleRate for >2 s, which would be a real bug).
            this.read = (this.read + 1) % this.RING;
          }
        }
        this.totalIn += d.length;
        if (this.priming && this.available >= this.PREFILL) {
          this.priming = false;
        }
      } else if (m.type === 'reset') {
        this.write = 0; this.read = 0; this.available = 0;
        this.readFrac = 0; this.priming = true;
        this.consecUnderruns = 0;
      } else if (m.type === 'stats') {
        this.port.postMessage({
          type: 'stats',
          available: this.available,
          underruns: this.underruns,
          totalOut: this.totalOut,
          totalIn: this.totalIn,
          priming: this.priming,
          ringRate: this.SRC_RATE,
          ctxRate: sampleRate,
        });
      }
    };
  }
  process(inputs, outputs) {
    const out = outputs[0][0];
    if (!out) return true;
    const n = out.length;
    if (this.priming) {
      // Hold silence until the ring is warm enough.
      for (let i = 0; i < n; i++) out[i] = 0;
      return true;
    }
    for (let i = 0; i < n; i++) {
      if (this.available <= 1) {
        // Absorb the empty slot as a single silence sample and keep
        // going -- single-sample underruns are inaudible at 8 kHz.
        // BUT: if underruns persist long enough to cross
        // UNDERRUN_REPRIME samples (~100 ms), re-enter priming so
        // we don't play the next tiny burst as a 20 ms fragment
        // when more data finally arrives. This is the
        // audible-stutter mode you hear when /ws/audio is
        // starved by dashboard polling.
        out[i] = 0;
        this.underruns++;
        this.consecUnderruns++;
        if (this.consecUnderruns >= this.UNDERRUN_REPRIME) {
          this.priming = true;
          this.consecUnderruns = 0;
          // Zero out the rest of this quantum then return -- the
          // next process() call will see priming=true and stay
          // silent until the ring refills to PREFILL.
          for (let j = i + 1; j < n; j++) out[j] = 0;
          return true;
        }
        continue;
      }
      this.consecUnderruns = 0;
      const a = this.ring[this.read];
      const nextRead = (this.read + 1) % this.RING;
      const b = this.ring[nextRead];
      out[i] = a + (b - a) * this.readFrac;
      this.readFrac += this.ratio;
      while (this.readFrac >= 1) {
        this.readFrac -= 1;
        this.read = (this.read + 1) % this.RING;
        this.available--;
        if (this.available <= 0) break;
      }
      this.totalOut++;
    }
    return true;
  }
}
registerProcessor('p25-audio', P25AudioProcessor);
`;

const AUDIO = {
  ctx: null,
  node: null,         // AudioWorkletNode OR ScriptProcessorNode
  gain: null,
  ws: null,
  playing: false,
  mode: null,         // 'worklet' | 'spn'
  chunks: 0,          // WS frames received this session
  underruns: 0,       // mirrored from worklet or read from AUDIO_SPN
  bufMs: 0,           // available samples converted to ms at 8 kHz
  lastChunkAt: 0,     // wall-clock ms of last arriving WS frame
  statsTimer: null,   // periodic port.postMessage({type:'stats'})
  ctxRate: 0,         // realized AudioContext sample rate
};

// ── ScriptProcessorNode fallback state ──
// Insecure-context browsers (http:// to a plain IP, which is how the
// dashboard is actually reached on the Fishball's direct-connect
// Ethernet) return `undefined` for `BaseAudioContext.audioWorklet`.
// In that case the worklet path isn't available, so we fall back to a
// ScriptProcessorNode running the same ring-buffer / linear-interp
// logic on the main thread. ScriptProcessorNode is spec-deprecated but
// still supported universally and has no secure-context gate. Audio
// quality is identical — same 8 kHz ring, same continuous resample
// to the context rate, same silence-on-underrun policy.
const AUDIO_SPN = {
  RING_SIZE: 16384,     // 2 s @ 8 kHz
  // 540 ms warm-up = 3 LDUs of headroom; see worklet PREFILL comment
  // above for the full reasoning. Shares the same tuning.
  PREFILL: 4320,
  // Re-prime after ~100 ms of sustained underruns at a typical
  // 48 kHz AudioContext. Same policy as the worklet path.
  UNDERRUN_REPRIME: 4800,
  ring: null,
  write: 0,
  read: 0,
  available: 0,
  readFrac: 0,
  ratio: 1.0,           // 8000 / ctxRate, set at startAudio
  underruns: 0,
  consecUnderruns: 0,
  priming: true,
};
function audioSpnReset() {
  AUDIO_SPN.ring = new Float32Array(AUDIO_SPN.RING_SIZE);
  AUDIO_SPN.write = 0;
  AUDIO_SPN.read = 0;
  AUDIO_SPN.available = 0;
  AUDIO_SPN.readFrac = 0;
  AUDIO_SPN.underruns = 0;
  AUDIO_SPN.consecUnderruns = 0;
  AUDIO_SPN.priming = true;
}
function audioSpnWrite(f32) {
  const s = AUDIO_SPN;
  for (let i = 0; i < f32.length; i++) {
    s.ring[s.write] = f32[i];
    s.write = (s.write + 1) % s.RING_SIZE;
    if (s.available < s.RING_SIZE) {
      s.available++;
    } else {
      // Ring full — drop oldest (should be unreachable on a healthy LAN)
      s.read = (s.read + 1) % s.RING_SIZE;
    }
  }
  if (s.priming && s.available >= s.PREFILL) s.priming = false;
}
function audioSpnProcess(e) {
  const out = e.outputBuffer.getChannelData(0);
  const n = out.length;
  const s = AUDIO_SPN;
  if (s.priming) {
    for (let i = 0; i < n; i++) out[i] = 0;
    return;
  }
  for (let i = 0; i < n; i++) {
    if (s.available <= 1) {
      // Absorb individual-sample underruns silently; after
      // UNDERRUN_REPRIME consecutive ones (~100 ms at 48 kHz)
      // re-enter priming so the next LDU burst buffers up to
      // PREFILL before playing, rather than stuttering out as a
      // 20 ms fragment.
      out[i] = 0;
      s.underruns++;
      s.consecUnderruns++;
      if (s.consecUnderruns >= s.UNDERRUN_REPRIME) {
        s.priming = true;
        s.consecUnderruns = 0;
        for (let j = i + 1; j < n; j++) out[j] = 0;
        return;
      }
      continue;
    }
    s.consecUnderruns = 0;
    const a = s.ring[s.read];
    const b = s.ring[(s.read + 1) % s.RING_SIZE];
    out[i] = a + (b - a) * s.readFrac;
    s.readFrac += s.ratio;
    while (s.readFrac >= 1) {
      s.readFrac -= 1;
      s.read = (s.read + 1) % s.RING_SIZE;
      s.available--;
      if (s.available <= 0) break;
    }
  }
}

function audioSetStatus(html) {
  $('audioStatus').innerHTML = html;
}

function audioUpdateStatus() {
  if (!AUDIO.playing) { audioSetStatus('stopped'); return; }
  // ScriptProcessor path doesn't use port.postMessage stats — pull
  // directly from the shared main-thread state.
  if (AUDIO.mode === 'spn') {
    AUDIO.bufMs = (AUDIO_SPN.available / 8) | 0;
    AUDIO.underruns = AUDIO_SPN.underruns;
  }
  const gapMs = AUDIO.lastChunkAt ? (Date.now() - AUDIO.lastChunkAt) : -1;
  let gapCls = 'on', gapTxt = 'live';
  if (gapMs < 0) {
    gapTxt = 'waiting for vocoder…';
    gapCls = 'warn';
  } else if (gapMs > 1500) {
    gapTxt = 'silent ' + (gapMs/1000).toFixed(1) + 's';
    gapCls = 'warn';
  }
  const rateTxt = AUDIO.ctxRate ? (AUDIO.ctxRate/1000).toFixed(1) + 'k' : '--';
  const modeTxt = AUDIO.mode === 'worklet' ? 'wkt'
               : AUDIO.mode === 'spn'     ? 'spn'
               : '--';
  audioSetStatus(
    '<span class="' + gapCls + '">' + gapTxt + '</span>'
    + '  &middot;  buf ' + AUDIO.bufMs + ' ms'
    + '  &middot;  rate ' + rateTxt
    + '  &middot;  ' + modeTxt
    + '  &middot;  chunks ' + AUDIO.chunks.toLocaleString()
    + (AUDIO.underruns ? '  &middot;  <span class="warn">underruns ' + AUDIO.underruns + '</span>' : '')
  );
}

function audioApplyGain() {
  if (!AUDIO.gain) return;
  const muted = $('audioMute').checked;
  const vol = parseInt($('audioVol').value, 10) / 100;
  AUDIO.gain.gain.value = muted ? 0 : vol;
}

function toggleAudio() {
  if (AUDIO.playing) { stopAudio(); } else { startAudio(); }
}

async function startAudio() {
  try {
    const Ctor = window.AudioContext || window.webkitAudioContext;
    if (!Ctor) { audioSetStatus('<span class="err">Web Audio unsupported</span>'); return; }
    // Don't pin sampleRate: let the browser pick native (usually 48 kHz).
    // The ring buffer + linear interp inside either the AudioWorklet
    // (secure context) or the ScriptProcessorNode fallback handles 8
    // kHz -> native conversion with one continuous interpolator, which
    // was the whole point of the 2026-04-15 rewrite.
    AUDIO.ctx = new Ctor();
    if (AUDIO.ctx.state === 'suspended') {
      try { await AUDIO.ctx.resume(); } catch {}
    }
    AUDIO.ctxRate = AUDIO.ctx.sampleRate;
    AUDIO.chunks = 0;
    AUDIO.underruns = 0;
    AUDIO.bufMs = 0;
    AUDIO.lastChunkAt = 0;

    // Mode selection:
    //   (a) Secure context (https:// or localhost) -> AudioWorklet.
    //       Runs the ring buffer on a dedicated audio thread, best
    //       real-time characteristics.
    //   (b) Insecure context (http:// to a LAN IP, which is the
    //       actual Fishball direct-connect setup) -> the browser
    //       returns `undefined` for `ctx.audioWorklet` because of the
    //       [SecureContext] IDL gate in the Web Audio spec. Fall back
    //       to ScriptProcessorNode, which is spec-deprecated but
    //       universally supported and runs the same ring buffer on
    //       the main thread. Quality is identical for 8 kHz P25
    //       voice; the only downside is main-thread jank sensitivity,
    //       and at 2 s refresh cadence the dashboard isn't blocking.
    if (AUDIO.ctx.audioWorklet) {
      AUDIO.mode = 'worklet';
      const blob = new Blob([AUDIO_WORKLET_CODE], {type:'application/javascript'});
      const url = URL.createObjectURL(blob);
      try {
        await AUDIO.ctx.audioWorklet.addModule(url);
      } finally {
        URL.revokeObjectURL(url);
      }
      AUDIO.node = new AudioWorkletNode(AUDIO.ctx, 'p25-audio');
      AUDIO.node.port.onmessage = (ev) => {
        const m = ev.data;
        if (m && m.type === 'stats') {
          AUDIO.bufMs = (m.available / 8) | 0;
          AUDIO.underruns = m.underruns;
        }
      };
    } else if (AUDIO.ctx.createScriptProcessor) {
      AUDIO.mode = 'spn';
      audioSpnReset();
      AUDIO_SPN.ratio = 8000 / AUDIO.ctxRate;
      // 1024 samples per callback = 21 ms at 48 kHz, 23 ms at 44.1 kHz.
      // Power-of-two required by ScriptProcessorNode; 1024 is the
      // sweet spot between latency and callback overhead for our
      // single-channel 8 kHz source.
      AUDIO.node = AUDIO.ctx.createScriptProcessor(1024, 0, 1);
      AUDIO.node.onaudioprocess = audioSpnProcess;
    } else {
      audioSetStatus('<span class="err">no usable audio path</span>');
      AUDIO.ctx.close(); AUDIO.ctx = null;
      return;
    }

    AUDIO.gain = AUDIO.ctx.createGain();
    AUDIO.node.connect(AUDIO.gain);
    AUDIO.gain.connect(AUDIO.ctx.destination);
    audioApplyGain();

    const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
    const wsUrl = proto + '//' + location.host + '/ws/audio';
    const ws = new WebSocket(wsUrl);
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
      AUDIO.playing = true;
      const btn = $('audioBtn');
      btn.classList.remove('play-off');
      btn.classList.add('play-on');
      btn.innerHTML = '&#9632; Stop Audio';
      if (!AUDIO.statsTimer) {
        AUDIO.statsTimer = setInterval(() => {
          if (AUDIO.mode === 'worklet' && AUDIO.node) {
            AUDIO.node.port.postMessage({type:'stats'});
          }
          audioUpdateStatus();
        }, 250);
      }
      audioUpdateStatus();
    };
    ws.onmessage = (ev) => {
      if (!(ev.data instanceof ArrayBuffer)) return;
      // 320 bytes = 160 × i16 LE = 20 ms of 8 kHz mono.
      const i16 = new Int16Array(ev.data);
      if (i16.length === 0 || !AUDIO.node) return;
      const f32 = new Float32Array(i16.length);
      for (let i = 0; i < i16.length; i++) f32[i] = i16[i] / 32768;
      if (AUDIO.mode === 'worklet') {
        // Transfer the buffer so there's no copy on the worklet side.
        AUDIO.node.port.postMessage({type:'pcm', data:f32}, [f32.buffer]);
      } else {
        audioSpnWrite(f32);
      }
      AUDIO.chunks++;
      AUDIO.lastChunkAt = Date.now();
    };
    ws.onclose = () => {
      if (AUDIO.playing) {
        audioSetStatus('<span class="warn">disconnected</span>');
        stopAudio();
      }
    };
    ws.onerror = () => {
      audioSetStatus('<span class="err">WS error</span>');
    };
    AUDIO.ws = ws;
  } catch (e) {
    audioSetStatus('<span class="err">' + e.message + '</span>');
    stopAudio();
  }
}

function stopAudio() {
  AUDIO.playing = false;
  try { if (AUDIO.ws) AUDIO.ws.close(); } catch {}
  AUDIO.ws = null;
  try { if (AUDIO.node) AUDIO.node.disconnect(); } catch {}
  // ScriptProcessorNode keeps its onaudioprocess closure alive until
  // the node is GC'd, which can prevent AudioContext shutdown. Null
  // the callback explicitly so the node is inert even if something
  // else holds a reference briefly.
  if (AUDIO.node && 'onaudioprocess' in AUDIO.node) {
    try { AUDIO.node.onaudioprocess = null; } catch {}
  }
  AUDIO.node = null;
  AUDIO.mode = null;
  try { if (AUDIO.gain) AUDIO.gain.disconnect(); } catch {}
  AUDIO.gain = null;
  try { if (AUDIO.ctx) AUDIO.ctx.close(); } catch {}
  AUDIO.ctx = null;
  AUDIO.ctxRate = 0;
  if (AUDIO.statsTimer) { clearInterval(AUDIO.statsTimer); AUDIO.statsTimer = null; }
  const btn = $('audioBtn');
  btn.classList.remove('play-on');
  btn.classList.add('play-off');
  btn.innerHTML = '&#9654; Play Audio';
  audioSetStatus('stopped');
}

// Volume / mute react immediately (script sits at bottom of body)
$('audioVol').addEventListener('input', audioApplyGain);
$('audioMute').addEventListener('change', audioApplyGain);

// ── Tab switching (Radio / Logs / Debug) ──
// All tab panes stay in the DOM and get updated by the 2 s refresh
// loop regardless of which tab is visible, so switching is instant
// (just a display:none toggle). Last-selected tab persists in
// localStorage.
function switchTab(name) {
  const panes = document.querySelectorAll('.tab-pane');
  panes.forEach(p => p.classList.toggle('active', p.id === 'tab-' + name));
  const buttons = document.querySelectorAll('#tabNav button');
  buttons.forEach(b => b.classList.toggle('active', b.dataset.tab === name));
  localStorage.setItem('p25_tab', name);
  if (name === 'logs') { LOGS.unread = 0; logRenderBadge(); }
}
(function() {
  const saved = localStorage.getItem('p25_tab');
  if (saved === 'radio' || saved === 'debug' || saved === 'logs') switchTab(saved);
})();

// ── Event log tail (Phase 7F.1 Logs tab) ──
// Polls GET /api/log?since=<last_seen_seq>&limit=200 on a 1 s cadence.
// Renders entries into #logViewer in chronological order (oldest at
// top) up to a rolling cap. Category filter chips are client-side so
// toggling is instant.
const LOGS = {
  lastSeq: 0,
  entries: [],
  cap: 500,
  unread: 0,
  timer: null,
};

function logRenderBadge() {
  const b = $('logs_badge');
  if (!b) return;
  b.textContent = LOGS.unread > 0 ? '(' + LOGS.unread + ')' : '';
  b.style.color = LOGS.unread > 0 ? 'var(--orange)' : '';
}

function logFilterActive() {
  return {
    grant:   $('logCatGrant').checked,
    traffic: $('logCatTraffic').checked,
    imbe:    $('logCatImbe').checked,
    vocoder: $('logCatVocoder').checked,
    system:  $('logCatSystem').checked,
  };
}

function logFmtTs(ms) {
  const d = new Date(ms);
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  const mmm = String(d.getMilliseconds()).padStart(3, '0');
  return hh + ':' + mm + ':' + ss + '.' + mmm;
}

function logFmtFields(f) {
  if (!f || typeof f !== 'object') return '';
  const parts = [];
  for (const k of Object.keys(f)) {
    let v = f[k];
    if (v === null || v === undefined) continue;
    if (typeof v === 'object') v = JSON.stringify(v);
    parts.push(k + '=' + v);
  }
  return parts.length ? '{' + parts.join(' ') + '}' : '';
}

function logRender() {
  const viewer = $('logViewer');
  if (!viewer) return;
  const filt = logFilterActive();
  const visible = LOGS.entries.filter(e => filt[e.category] !== false);
  // Build DOM in one pass; for ~500 entries this is fine every 1 s.
  const html = visible.map(e => {
    const fields = logFmtFields(e.fields);
    const esc = s => String(s).replace(/[&<>"']/g, c => ({
      '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;'
    }[c]));
    return '<div class="log-entry">' +
      '<span class="log-ts">' + logFmtTs(e.timestamp_ms) + '</span>' +
      '<span class="log-cat ' + e.category + '">' + e.category + '</span>' +
      '<span class="log-body">' + esc(e.message) +
        (fields ? '<span class="log-fields">' + esc(fields) + '</span>' : '') +
      '</span>' +
    '</div>';
  }).join('');
  viewer.innerHTML = html;
  if ($('logAutoscroll').checked) {
    viewer.scrollTop = viewer.scrollHeight;
  }
  $('logStatus').textContent =
    LOGS.entries.length + ' entries · ' +
    visible.length + ' shown · last_seq=' + LOGS.lastSeq;
}

async function logPoll() {
  try {
    const r = await fetch('/api/log?since=' + LOGS.lastSeq + '&limit=200');
    if (!r.ok) return;
    const d = await r.json();
    if (!d || !Array.isArray(d.entries)) return;
    if (d.entries.length > 0) {
      LOGS.entries.push(...d.entries);
      while (LOGS.entries.length > LOGS.cap) LOGS.entries.shift();
      LOGS.lastSeq = d.last_seq;
      // Badge: count entries received while Logs tab isn't active
      const activeTab = document.querySelector('#tabNav button.active');
      if (!activeTab || activeTab.dataset.tab !== 'logs') {
        LOGS.unread += d.entries.length;
        logRenderBadge();
      }
      logRender();
    }
  } catch {}
}

function logClear() {
  LOGS.entries = [];
  LOGS.unread = 0;
  logRenderBadge();
  logRender();
}

// Filter checkbox changes re-render immediately
['logCatGrant', 'logCatTraffic', 'logCatImbe', 'logCatVocoder', 'logCatSystem']
  .forEach(id => {
    const el = $(id);
    if (el) el.addEventListener('change', logRender);
  });

// Log poller runs at 1 s when the Logs tab is active (so the tail
// updates smoothly as the user reads it) and 5 s otherwise (enough
// to keep the unread-count badge on the tab button current without
// hammering /api/log from every tab). Cadence is reset on every
// tab switch via the switchTab hook below.
function logPollCadence() {
  return activeTab() === 'logs' ? 1000 : 5000;
}
function logPollStartTimer() {
  if (LOGS.timer) clearInterval(LOGS.timer);
  LOGS.timer = setInterval(logPoll, logPollCadence());
}
logPollStartTimer();
logPoll(); // kick off immediately

// Spectrum renderer. Polls /api/spectrum at the rate chosen by the
// dropdown. Paints the FFT to a <canvas>, with optional peak-hold
// overlay so slow-moving interferers are visible against the noise
// floor. Only runs when the Debug tab is active + rate != Paused.
let SPEC = {
  timer: null,
  peak: null,
  rate: 1000,
};
function resetSpectrum() {
  SPEC.peak = null;
}
function scheduleSpectrum() {
  if (SPEC.timer) { clearInterval(SPEC.timer); SPEC.timer = null; }
  const r = parseInt($('spec_rate').value, 10);
  SPEC.rate = r;
  if (r > 0) {
    SPEC.timer = setInterval(refreshSpectrum, r);
    refreshSpectrum(); // kick off immediately
  }
}
async function refreshSpectrum() {
  // Don't poll when the tab is hidden — pointless CPU on both sides.
  const pane = $('tab-debug');
  if (!pane || pane.style.display === 'none') return;
  const chain = $('spec_chain').value || 'control';
  const status = $('spec_status');
  status.textContent = 'fetching...';
  const data = await fetchJson('/api/spectrum?chain=' + encodeURIComponent(chain));
  if (!data || !data.ok || !Array.isArray(data.mag_db)) {
    status.textContent = (data && data.error) || 'no data';
    status.style.color = 'var(--red)';
    return;
  }
  status.style.color = 'var(--text-dim)';
  const center_mhz = (data.center_hz || 0) / 1e6;
  const span_hz = data.sample_rate_hz || 62500;
  status.textContent = `${chain} @ ${center_mhz.toFixed(4)} MHz, ${(span_hz/1000).toFixed(1)} kHz span`;
  drawSpectrum(data.mag_db, data.center_hz, span_hz);
}
function drawSpectrum(mag_db, center_hz, span_hz) {
  const c = $('spec_canvas');
  if (!c) return;
  const ctx = c.getContext('2d');
  const w = c.width, h = c.height;
  ctx.clearRect(0, 0, w, h);

  // Peak-hold accumulate.
  if ($('spec_peak_hold').checked) {
    if (!SPEC.peak || SPEC.peak.length !== mag_db.length) {
      SPEC.peak = mag_db.slice();
    } else {
      for (let i = 0; i < mag_db.length; i++) {
        if (mag_db[i] > SPEC.peak[i]) SPEC.peak[i] = mag_db[i];
      }
    }
  } else {
    SPEC.peak = null;
  }

  // Y-axis: fixed dB range — dBFS against i16 full-scale.
  // Noise floor on a quiet post-DDC signal tends to sit near -80 dB,
  // strongest P25 channels peak around -30 to -40 dB. Show -100 to 0.
  const yMin = -100, yMax = 0;
  const mapY = db => h - ((db - yMin) / (yMax - yMin)) * h;

  // Grid: 10 dB horizontal lines.
  ctx.strokeStyle = '#1e2a3a';
  ctx.lineWidth = 1;
  for (let db = yMin; db <= yMax; db += 10) {
    const y = mapY(db);
    ctx.beginPath();
    ctx.moveTo(0, y);
    ctx.lineTo(w, y);
    ctx.stroke();
  }
  // Grid: vertical lines at every 5 kHz offset.
  const half_khz = span_hz / 2000;
  const step_khz = 5;
  for (let khz = -half_khz; khz <= half_khz; khz += step_khz) {
    const x = ((khz + half_khz) / (half_khz * 2)) * w;
    ctx.beginPath();
    ctx.moveTo(x, 0);
    ctx.lineTo(x, h);
    ctx.stroke();
  }

  // Center tick (channel center).
  ctx.strokeStyle = '#60a5fa';
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(w / 2, 0);
  ctx.lineTo(w / 2, h);
  ctx.stroke();

  // Peak-hold (drawn first so live trace covers it).
  if (SPEC.peak) {
    ctx.strokeStyle = '#f59e0b';
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let i = 0; i < SPEC.peak.length; i++) {
      const x = (i / (SPEC.peak.length - 1)) * w;
      const y = mapY(SPEC.peak[i]);
      if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
    }
    ctx.stroke();
  }

  // Live trace.
  ctx.strokeStyle = '#22c55e';
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let i = 0; i < mag_db.length; i++) {
    const x = (i / (mag_db.length - 1)) * w;
    const y = mapY(mag_db[i]);
    if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
  }
  ctx.stroke();

  // Axis labels.
  ctx.fillStyle = '#9ca3af';
  ctx.font = '10px system-ui,sans-serif';
  for (let db = yMin; db <= yMax; db += 20) {
    ctx.fillText(`${db} dB`, 4, mapY(db) - 2);
  }
  const centerMhz = (center_hz || 0) / 1e6;
  ctx.fillText(
    `${centerMhz.toFixed(4)} MHz`,
    w / 2 - 30, h - 4
  );
  ctx.fillText(
    `-${half_khz.toFixed(1)} kHz`,
    4, h - 4
  );
  ctx.fillText(
    `+${half_khz.toFixed(1)} kHz`,
    w - 60, h - 4
  );
}
// Lazy-start the poller on first Debug-tab activation, and STOP
// both pollers when any other tab becomes active. Previously the
// timers kept firing regardless of the visible tab; the
// `refreshConstellation` / `refreshSpectrum` bodies had an
// "is tab visible" early-return, but they used `pane.style.display
// === 'none'` which never matches the `classList.toggle('active')`
// model switchTab() actually uses -- so the timers kept burning
// /api/constellation + /api/spectrum round-trips at 1 Hz on every
// tab. That starved /ws/audio on the Zynq-7020 ARM and caused
// audible mid-call stutter in the live-audio stream.
//
// Stopping the timers on tab exit is correct: a setInterval doing
// real work during an invisible tab is pure waste on both client
// and server. The next activation relaunches them.
const _origSwitchTabForSpec = switchTab;
switchTab = function(name) {
  _origSwitchTabForSpec(name);
  if (name === 'debug') {
    if (!SPEC.timer && SPEC.rate > 0) scheduleSpectrum();
    if (!IQ.timer && IQ.rate > 0) scheduleConstellation();
  } else {
    if (SPEC.timer) { clearInterval(SPEC.timer); SPEC.timer = null; }
    if (IQ.timer)   { clearInterval(IQ.timer);   IQ.timer   = null; }
  }
  // Reset the log-poll cadence so switching into Logs speeds it
  // up to 1 s immediately; switching out slows to 5 s.
  if (typeof logPollStartTimer === 'function') logPollStartTimer();
  // Kick the Radio-tab refreshers when entering Radio so the
  // user sees fresh data instead of waiting for the next tick.
  if (name === 'radio') {
    if (typeof refresh === 'function') refresh();
    if (typeof refreshRecordings === 'function') refreshRecordings();
    if (typeof refreshModulation === 'function') refreshModulation();
    if (typeof refreshMonitorTgs === 'function') refreshMonitorTgs();
  }
};
// At load time the SPEC/IQ timers haven't been scheduled yet
// (scheduleSpectrum / scheduleConstellation only fire on a user
// switch to Debug via the switchTab override above), so no
// cleanup IIFE is needed here. The originally-drafted version
// accessed `IQ.timer` before `let IQ = {...}` was declared --
// temporal-dead-zone ReferenceError that silently broke every
// subsequent `setInterval(refresh, 2000)` etc.

// Constellation scatter. Pulls post-PLL (I, Q) points from
// /api/constellation, draws a 4-quadrant scatter with optional
// persistence fade so moving clouds leave a trail.
let IQ = {
  timer: null,
  rate: 1000,
  lastI: null,
  lastQ: null,
};
function scheduleConstellation() {
  if (IQ.timer) { clearInterval(IQ.timer); IQ.timer = null; }
  const r = parseInt($('iq_rate').value, 10);
  IQ.rate = r;
  if (r > 0) {
    IQ.timer = setInterval(refreshConstellation, r);
    refreshConstellation();
  }
}
async function refreshConstellation() {
  const pane = $('tab-debug');
  if (!pane || pane.style.display === 'none') return;
  const chain = $('iq_chain').value || 'traffic';
  const status = $('iq_status');
  status.textContent = 'fetching...';
  const data = await fetchJson('/api/constellation?chain=' + encodeURIComponent(chain));
  if (!data || !data.ok || !Array.isArray(data.i)) {
    status.textContent = (data && data.error) || 'no data';
    status.style.color = 'var(--red)';
    return;
  }
  status.style.color = 'var(--text-dim)';
  status.textContent = `${chain}: ${data.count} points, pll=${(data.pll_final||0).toFixed(3)} rad, timing=${(data.timing_final||0).toFixed(2)} samp`;
  IQ.lastI = data.i;
  IQ.lastQ = data.q;
  drawConstellation(data.i, data.q);
}
function drawConstellation(iArr, qArr) {
  const c = $('iq_canvas');
  if (!c) return;
  const ctx = c.getContext('2d');
  const w = c.width, h = c.height;
  // Persistence fade: paint a translucent black layer over the
  // previous frame so old points decay. When unchecked, fully
  // erase the canvas each tick.
  const persist = $('iq_persistence').checked;
  if (persist) {
    ctx.fillStyle = 'rgba(10, 15, 26, 0.22)';
    ctx.fillRect(0, 0, w, h);
  } else {
    ctx.clearRect(0, 0, w, h);
    ctx.fillStyle = '#0a0f1a';
    ctx.fillRect(0, 0, w, h);
  }
  // Axes centered. Range ±1.6 nominal (LSM soft symbols sit near
  // ±1, ±j; allow headroom for over-amplified points).
  const axisMax = 1.6;
  const mapX = re => (re + axisMax) / (axisMax * 2) * w;
  const mapY = im => h - (im + axisMax) / (axisMax * 2) * h;

  // Grid.
  ctx.strokeStyle = '#1e2a3a';
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, h / 2); ctx.lineTo(w, h / 2);
  ctx.moveTo(w / 2, 0); ctx.lineTo(w / 2, h);
  ctx.stroke();
  // Unit circle.
  ctx.strokeStyle = '#2b3b55';
  ctx.beginPath();
  ctx.arc(w / 2, h / 2, w / 2 / axisMax, 0, Math.PI * 2);
  ctx.stroke();
  // Expected cluster centers for P25 LSM (π/4-DQPSK). Decision
  // points sit at ±π/4 and ±3π/4, i.e. (±1/√2, ±1/√2). Earlier
  // version of this code placed them at (±1, 0)/(0, ±j) which is
  // QPSK convention and doesn't match P25's actual symbol phases.
  const r4 = Math.SQRT1_2;
  ctx.fillStyle = 'rgba(96, 165, 250, 0.4)';
  for (const [cx, cy] of [[r4, r4], [-r4, r4], [-r4, -r4], [r4, -r4]]) {
    ctx.beginPath();
    ctx.arc(mapX(cx), mapY(cy), 3, 0, Math.PI * 2);
    ctx.fill();
  }

  // Points. Use a soft green so persistence builds up nicely.
  ctx.fillStyle = 'rgba(34, 197, 94, 0.55)';
  for (let k = 0; k < iArr.length; k++) {
    const x = mapX(iArr[k]);
    const y = mapY(qArr[k]);
    ctx.fillRect(x - 1, y - 1, 2, 2);
  }

  // Labels.
  ctx.fillStyle = '#9ca3af';
  ctx.font = '10px system-ui,sans-serif';
  ctx.fillText('+I', w - 18, h / 2 - 4);
  ctx.fillText('+Q', w / 2 + 4, 12);
  ctx.fillText('-I', 4, h / 2 - 4);
  ctx.fillText('-Q', w / 2 + 4, h - 4);
}

// Modulation selector. GET polls current mode + per-decoder rates
// so the user can see why auto picked what it did. PUT on change
// forces one of {c4fm, lsm, auto}.
async function refreshModulation() {
  // Drives the Board Info "System type" + modulation selector on Radio only.
  if (activeTab() !== 'radio') return;
  const data = await fetchJson('/api/modulation');
  if (!data) return;
  const sel = $('bi_mod_sel');
  const status = $('bi_mod_status');
  const rates = data.nid_decoded_ok || {};
  const c = rates.c4fm || 0;
  const l = rates.lsm || 0;
  const label = data.label || '--';
  status.textContent = `${label} (c4fm NIDs=${c.toLocaleString()} / lsm NIDs=${l.toLocaleString()})`;
  // Mirror to the prominent Board Info "System type" field. Combines
  // the live modulation label (from /api/modulation) with the P25
  // phase label (from /api/system, inferred from IDEN_UPDATE_TDMA
  // TSBK presence). Example output: "LSM · P25 P1+P2".
  const sysEl = $('bi_sys_type');
  if (sysEl) {
    // Pull the last-cached /api/system response off window so we
    // don't need an extra fetch. `refresh()` updates it every 2 s.
    const phase = window._LAST_SYS_PHASE || '';
    sysEl.textContent = phase ? `${label} \u00B7 ${phase}` : label;
  }
  // Only overwrite the dropdown if the server state actually
  // diverges (avoids fighting the user mid-click).
  const serverVal = ({0:'auto',1:'c4fm',2:'lsm'})[data.mode] || 'auto';
  if (sel && sel.value !== serverVal && document.activeElement !== sel) {
    sel.value = serverVal;
  }
}
async function setModulation(mode) {
  const status = $('bi_mod_status');
  status.textContent = `setting to ${mode}...`;
  const res = await fetchJson('/api/modulation?set=' + encodeURIComponent(mode));
  if (res && res.ok) {
    status.style.color = 'var(--green)';
    refreshModulation();
  } else {
    status.style.color = 'var(--red)';
    status.textContent = (res && res.error) || 'failed';
  }
}

// TG Monitor (scanner-mode allow-list). Roster comes from the
// persistent grant_map; current filter state comes from
// /api/monitor (which drives the grant-follower gate in main.rs).
// Local state: a Set of TGs the USER has checked in this render.
// Rebuilt from /api/monitor on every refreshMonitorTgs() so the UI
// reflects the actual follower state after any out-of-band change.
window._TG_MON_STATE = {
  roster: [],   // [{tg, total, clear, enc, freqs:[{hz,count}]}]
  active: new Set(), // currently-applied allow-list
  staged: new Set(), // user's in-progress checkbox state
  loaded: false,
};
async function refreshMonitorTgs() {
  // TG Monitor picker is Radio-tab only.
  if (activeTab() !== 'radio') return;
  // Parallel fetch: grant_map for the roster, /api/monitor for the
  // applied filter.
  const [gm, mon] = await Promise.all([
    fetchJson('/api/grant_map'),
    fetchJson('/api/monitor'),
  ]);
  if (!gm || !mon) {
    const body = $('tgmon_body');
    if (body) body.innerHTML = '<span style="color:var(--red)">failed to load</span>';
    return;
  }
  // Roll (tg, freq) rows up to per-TG summaries.
  const byTg = new Map();
  for (const e of (gm.entries || [])) {
    const key = e.tg;
    const cur = byTg.get(key) || {tg: key, total: 0, clear: 0, enc: 0, freqs: []};
    cur.total += e.count;
    cur.clear += e.count - e.encrypted_count;
    cur.enc += e.encrypted_count;
    cur.freqs.push({hz: e.frequency_hz, count: e.count});
    byTg.set(key, cur);
  }
  const roster = Array.from(byTg.values());
  roster.sort((a, b) => b.total - a.total);
  window._TG_MON_STATE.roster = roster;
  window._TG_MON_STATE.active = new Set(mon.talkgroups || []);
  // Seed staged from active on a fresh load so the UI shows the
  // current filter until the user starts editing.
  window._TG_MON_STATE.staged = new Set(mon.talkgroups || []);
  window._TG_MON_STATE.loaded = true;
  renderTgMonitor();
}

// DOM-node-reuse render for the TG Monitor picker.
//
// Previous implementation rebuilt `tgmon_body.innerHTML` on every
// refresh tick. That tore down checkboxes + labels even when the
// roster hadn't changed, causing visible flicker on the 10 s poll
// cadence and making active checkboxes momentarily un-responsive.
//
// New pattern: build <label> nodes once per TG, stash them in a
// Map<tg, HTMLLabelElement>, and on each render:
//   - add any newly-observed TGs as fresh nodes (appendChild)
//   - remove any evicted TGs (parentNode.removeChild)
//   - update badge text + checkbox.checked in place on existing
//     nodes without touching the surrounding DOM
// This is the same strategy we use for refreshRecordings'
// fingerprint-based skip; the difference is this picker needs
// per-item granularity because individual badges (clr, ENC, total)
// update as new grants land while the set of TGs is stable.
window._TG_MON_NODES = new Map();

function _tgMonBuildNode(tg) {
  const label = document.createElement('label');
  label.style.cssText = 'display:flex;align-items:center;gap:6px;padding:3px 6px;border:1px solid #1f2937;border-radius:4px';
  label.dataset.tg = tg;
  const cb = document.createElement('input');
  cb.type = 'checkbox';
  cb.dataset.tg = tg;
  cb.addEventListener('change', onTgMonitorCheck);
  const tgSpan = document.createElement('span');
  tgSpan.style.fontFamily = 'var(--mono)';
  tgSpan.textContent = 'TG ' + tg;
  const clearSpan = document.createElement('span');
  clearSpan.className = 'tgmon-clear';
  clearSpan.style.cssText = 'color:var(--green);font-size:0.8em';
  const encSpan = document.createElement('span');
  encSpan.className = 'tgmon-enc';
  encSpan.style.cssText = 'color:var(--red);font-size:0.8em';
  const totalSpan = document.createElement('span');
  totalSpan.className = 'tgmon-total';
  totalSpan.style.cssText = 'margin-left:auto;color:var(--text-dim);font-size:0.8em';
  label.appendChild(cb);
  label.appendChild(tgSpan);
  label.appendChild(clearSpan);
  label.appendChild(encSpan);
  label.appendChild(totalSpan);
  return label;
}

function _tgMonEmptyMessage() {
  const p = document.createElement('span');
  p.style.color = 'var(--text-dim)';
  p.dataset.empty = '1';
  p.textContent = 'No TGs observed yet. Wait for grants or uncheck "Hide encrypted TGs".';
  return p;
}

function renderTgMonitor() {
  const body = $('tgmon_body');
  const status = $('tgmon_status');
  if (!body || !status) return;
  const st = window._TG_MON_STATE;
  if (!st.loaded) {
    status.textContent = 'loading...';
    return;
  }
  const hideEnc = $('tgmon_hide_enc') && $('tgmon_hide_enc').checked;
  let roster = st.roster;
  if (hideEnc) {
    // Matches the backend behaviour: once a TG is observed encrypted
    // even once, it's added to `encrypted_tg_history` and all future
    // grants are blocked regardless of the per-TSBK encryption flag.
    // So any TG with `enc > 0` is effectively unusable for monitoring.
    roster = roster.filter(r => r.enc === 0);
  }

  // Build the set of TGs we *want* visible right now.
  const visible = new Set(roster.map(r => r.tg));

  // Remove nodes whose TG is no longer visible.
  for (const [tg, node] of window._TG_MON_NODES.entries()) {
    if (!visible.has(tg)) {
      if (node.parentNode) node.parentNode.removeChild(node);
      window._TG_MON_NODES.delete(tg);
    }
  }
  // Remove any "empty" placeholder message if we now have rows.
  const placeholder = body.querySelector('[data-empty="1"]');
  if (placeholder && roster.length > 0) placeholder.remove();

  if (roster.length === 0) {
    if (!placeholder) {
      body.innerHTML = '';
      body.appendChild(_tgMonEmptyMessage());
    }
  } else {
    // Add + update per roster.
    for (const r of roster) {
      let node = window._TG_MON_NODES.get(r.tg);
      if (!node) {
        node = _tgMonBuildNode(r.tg);
        window._TG_MON_NODES.set(r.tg, node);
        body.appendChild(node);
      }
      // Update badges + checkbox without replacing the DOM.
      const cb = node.querySelector('input[type=checkbox]');
      if (cb && cb.checked !== st.staged.has(r.tg)) {
        cb.checked = st.staged.has(r.tg);
      }
      const clearSpan = node.querySelector('.tgmon-clear');
      if (clearSpan) {
        clearSpan.textContent = r.clear > 0 ? (r.clear + ' clr') : '';
      }
      const encSpan = node.querySelector('.tgmon-enc');
      if (encSpan) {
        encSpan.textContent = r.enc > 0 ? ('[ENC ' + r.enc + ']') : '';
      }
      const totalSpan = node.querySelector('.tgmon-total');
      if (totalSpan) {
        totalSpan.textContent = String(r.total);
      }
    }
  }

  // Status line -- this is fine to rebuild since it has no
  // interactive elements.
  const active = Array.from(st.active).sort((a, b) => a - b);
  const staged = Array.from(st.staged).sort((a, b) => a - b);
  const dirty = active.length !== staged.length ||
                active.some((v, i) => v !== staged[i]);
  if (active.length === 0) {
    status.innerHTML = '<span style="color:var(--text-dim)">Filter: <b>accept-all</b></span>';
  } else {
    status.innerHTML = '<span>Filter active: ' + active.map(t => '<b>TG ' + t + '</b>').join(', ') + '</span>';
  }
  if (dirty) {
    status.innerHTML += ' <span style="color:var(--orange);margin-left:8px">(unsaved changes -- click Apply)</span>';
  }
}

function onTgMonitorCheck(ev) {
  const tg = parseInt(ev.target.getAttribute('data-tg'), 10);
  if (!Number.isFinite(tg)) return;
  if (ev.target.checked) {
    window._TG_MON_STATE.staged.add(tg);
  } else {
    window._TG_MON_STATE.staged.delete(tg);
  }
  renderTgMonitor();
}

async function applyMonitorTgs() {
  const staged = Array.from(window._TG_MON_STATE.staged);
  const body = JSON.stringify({talkgroups: staged});
  try {
    const resp = await fetch('/api/monitor', {
      method: 'PUT',
      headers: {'Content-Type': 'application/json'},
      body,
    });
    const data = await resp.json();
    window._TG_MON_STATE.active = new Set(data.talkgroups || []);
    window._TG_MON_STATE.staged = new Set(data.talkgroups || []);
    renderTgMonitor();
  } catch (e) {
    const status = $('tgmon_status');
    if (status) {
      status.innerHTML = '<span style="color:var(--red)">apply failed: ' + e + '</span>';
    }
  }
}

function clearMonitorTgs() {
  window._TG_MON_STATE.staged = new Set();
  renderTgMonitor();
}

// Recordings ring. Poll less often than refresh() — new calls
// finalise at human-speech cadence so 5 s is plenty, and each WAV
// payload is kB-scale (header + metadata only, audio blobs are
// fetched on-demand by the <audio> element). Renders a small table
// with inline <audio controls> so playback is one click.
// Cached fingerprint of the last-rendered recording list so we can
// skip re-render when nothing changed. Prevents the `<audio>` tags
// from being torn down every 5 s, which was interrupting playback.
window._REC_LAST_FP = '';

async function refreshRecordings() {
  // Recordings card is Radio-tab only.
  if (activeTab() !== 'radio') return;
  const data = await fetchJson('/api/recordings');
  if (!data) return;
  const count = data.count || 0;
  const max = data.max || 0;
  $('rec_count').textContent = count
    ? `${count} / ${max}`
    : `0 / ${max}`;
  const tbody = $('rec_tbody');
  if (!Array.isArray(data.items) || data.items.length === 0) {
    if (window._REC_LAST_FP !== 'empty') {
      tbody.innerHTML = '<tr><td colspan="5" style="color:var(--text-dim)">No recordings yet.</td></tr>';
      window._REC_LAST_FP = 'empty';
    }
    return;
  }

  // Fingerprint = ordered list of IDs + size_bytes per row. If the
  // server hasn't added or evicted anything since our last render,
  // skip the whole innerHTML rebuild so any in-flight `<audio>`
  // element keeps its playback state. We include size_bytes in the
  // fingerprint so an in-progress recording that's still growing
  // also triggers a refresh (the size changes as the WAV is
  // finalized).
  const fp = data.items.map(i => `${i.id}:${i.size_bytes}`).join(',');
  if (fp === window._REC_LAST_FP) return;

  // List changed. Before tearing down, capture any currently-playing
  // `<audio>` elements + their playback position so we can restore
  // them after the rebuild -- the user shouldn't lose playback just
  // because a new recording appeared or the oldest was evicted.
  const playingState = new Map(); // id -> {currentTime, volume, muted}
  tbody.querySelectorAll('audio').forEach(a => {
    if (!a.paused && !a.ended) {
      const src = a.getAttribute('src') || '';
      const m = src.match(/\/recordings\/(\d+)/);
      if (m) {
        playingState.set(m[1], {
          currentTime: a.currentTime,
          volume: a.volume,
          muted: a.muted,
        });
      }
    }
  });

  const fmtDur = ms => {
    const s = Math.floor(ms / 1000);
    const m = Math.floor(s / 60);
    const sec = s % 60;
    return m > 0
      ? `${m}m ${sec.toString().padStart(2, '0')}s`
      : `${s}.${Math.floor((ms % 1000) / 100)}s`;
  };
  const fmtSize = b => {
    if (b < 1024) return `${b} B`;
    if (b < 1_048_576) return `${(b / 1024).toFixed(1)} KB`;
    return `${(b / 1_048_576).toFixed(2)} MB`;
  };
  const fmtClock = ms => {
    if (!ms) return '--';
    const d = new Date(ms);
    const hh = d.getHours().toString().padStart(2, '0');
    const mm = d.getMinutes().toString().padStart(2, '0');
    const ss = d.getSeconds().toString().padStart(2, '0');
    return `${hh}:${mm}:${ss}`;
  };

  const rows = data.items.map(it => {
    const url = `/api/recordings/${it.id}.wav`;
    return `<tr data-rec-id="${it.id}">
      <td>${fmtClock(it.started_unix_ms)}</td>
      <td>${it.talkgroup || '--'}</td>
      <td>${fmtDur(it.duration_ms)}</td>
      <td>${fmtSize(it.size_bytes)}</td>
      <td><audio controls preload="none" style="height:28px" src="${url}"></audio>
          <a href="${url}" download style="margin-left:6px;font-size:0.85em">⬇</a></td>
    </tr>`;
  });
  tbody.innerHTML = rows.join('');
  window._REC_LAST_FP = fp;

  // Restore playback for any recordings that were playing before
  // the rebuild. We use `play()` + seek to the saved currentTime
  // rather than trying to preserve the DOM node, because a row's
  // position may have changed (new recording pushed it down) and
  // cloneNode can't transfer the decoded buffer anyway.
  if (playingState.size > 0) {
    tbody.querySelectorAll('tr').forEach(tr => {
      const id = tr.getAttribute('data-rec-id');
      const st = playingState.get(id);
      if (!st) return;
      const audio = tr.querySelector('audio');
      if (!audio) return;
      audio.volume = st.volume;
      audio.muted = st.muted;
      // Seek-then-play. `play()` returns a promise that resolves
      // once the seek completes; ignore rejections (user might
      // have started a different playback in the meantime).
      audio.currentTime = st.currentTime;
      audio.play().catch(() => {});
    });
  }
}

// API catalogue. Fetch once on first render of the API tab; cheap
// enough to also refresh on each tab switch so a rebuild with new
// routes updates the table without a page reload.
let API_LOADED = false;
let API_ITEMS = [];
function renderApi() {
  const tbody = $('api_tbody');
  const filter = ($('api_filter').value || '').toLowerCase();
  if (!API_ITEMS.length) {
    tbody.innerHTML = '<tr><td colspan="5" style="color:var(--text-dim)">No endpoints.</td></tr>';
    return;
  }
  const rows = API_ITEMS
    .filter(it => {
      if (!filter) return true;
      return (it.path && it.path.toLowerCase().includes(filter))
          || (it.description && it.description.toLowerCase().includes(filter))
          || (it.params && it.params.toLowerCase().includes(filter));
    })
    .map(it => {
      const isGet = it.method === 'GET';
      // Inline "Try" link only for zero-param GET (clicking a URL
      // with query params would error; the user needs to edit first).
      const cleanPath = isGet && !it.params
        ? `<a href="${it.path}" target="_blank" title="open JSON in new tab">Open</a>`
        : '<span style="color:var(--text-dim)">--</span>';
      return `<tr>
        <td><code>${it.method}</code></td>
        <td><code>${it.path}</code></td>
        <td style="font-family:monospace;font-size:0.85em;color:var(--text-dim)">${it.params || ''}</td>
        <td>${it.description || ''}</td>
        <td>${cleanPath}</td>
      </tr>`;
    });
  tbody.innerHTML = rows.length
    ? rows.join('')
    : '<tr><td colspan="5" style="color:var(--text-dim)">No match.</td></tr>';
}
async function loadApiCatalogue() {
  const data = await fetchJson('/api/endpoints');
  if (!data || !Array.isArray(data.items)) return;
  API_ITEMS = data.items;
  API_LOADED = true;
  renderApi();
}
// Wire the filter input.
document.addEventListener('DOMContentLoaded', () => {
  const f = $('api_filter');
  if (f) f.addEventListener('input', renderApi);
});
// Lazy-load on first tab activation so we don't pull 25 lines of
// JSON on every page load.
const origSwitchTab = switchTab;
switchTab = function(name) {
  origSwitchTab(name);
  if (name === 'api' && !API_LOADED) loadApiCatalogue();
};

// Browser-pushed wall-clock sync. On isolated networks (RNDIS,
// air-gapped) the board's NTP-on-boot can't reach a real server,
// so we ship it whatever time this browser has. Accuracy is
// bounded by HTTP round-trip jitter (typically <100 ms), which is
// fine for event-log ordering and the /api/stats wall_clock field.
// Fire-and-forget; log the result to console but don't block UI.
async function syncBoardTime() {
  try {
    const ms = Date.now();
    const r = await fetch('/api/set_time?unix_ms=' + ms,
                         { method: 'POST' });
    const j = await r.json();
    if (j && j.ok) {
      console.log('board time synced:', new Date(ms).toISOString());
    } else {
      console.warn('board time sync failed:', j);
    }
  } catch (e) {
    console.warn('board time sync error:', e);
  }
}

loadAliases();
syncBoardTime();  // fire-and-forget: isolated-network NTP fallback
refresh();
setInterval(refresh, 2000);
refreshRecordings();
setInterval(refreshRecordings, 5000);
refreshModulation();
setInterval(refreshModulation, 3000);
// Phase 10-prep: TG monitor picker. Grant map grows over time so
// we also refresh the roster on a slower cadence than the main
// poll. The monitor filter state rarely changes (user clicks
// Apply explicitly) but re-reading it keeps the UI in sync with
// any out-of-band /api/monitor edits.
refreshMonitorTgs();
setInterval(refreshMonitorTgs, 10000);
connectWs();
</script>
</body>
</html>"##;
