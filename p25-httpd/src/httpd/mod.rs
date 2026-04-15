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
    routing::get,
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
        .route("/api/lsm_dibit_dump", get(get_lsm_dibit_dump))
        .route("/api/lsm_capture", get(get_lsm_capture))
        .route("/api/lsm_capture_aligned", get(get_lsm_capture_aligned))
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
        // Phase 6G.2: runtime read/write of the lsm_control register
        // (lsm_enable, lsm_dibit_dma_enable, lsm_dc_block_enable). The
        // dc_block_enable bit is the runtime A/B knob the doc 030 PL
        // port roadmap wanted -- previously had to be poked via
        // ssh + devmem on the board.
        .route("/api/lsm_control", get(get_lsm_control))
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
        Ok(_) => applied.push(format!("rx_lo={rx_lo}")),
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

// ── REST Handlers ──────────────────────────────────────────────────────

async fn get_system(State(state): State<Arc<AppState>>) -> Json<SystemInfo> {
    // Phase 6F.11 API-level merge: read BOTH lsm decoders and pick
    // Phase 9 retirement: this handler used to union the
    // Phase 6D software LSM pipeline (`iq_lsm_decoder`) with the
    // PL-fed `lsm_decoder` via a `pick()` fallback. After the
    // software pipeline retired, the PL LSM chain is the single
    // source of truth for system identity. If this ever shows
    // stale data the right fix is to make `lsm_decoder` read
    // fresher, not to resurrect the software cross-check.
    let dec = state.lsm_decoder.read().await;
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

    let dec = state.lsm_decoder.read().await;
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
    let dec = state.lsm_decoder.read().await;
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
    let decoder = state.lsm_decoder.read().await;

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
async fn get_lsm_dibit_dump(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let decoder = state.lsm_decoder.read().await;
    Json(dibit_dump_json(&decoder, "PL HDL LSM chain (lsm_dibit_dma)"))
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
/// (`/api/lsm_capture_aligned`); this one just returns whatever's
/// currently in the rolling buffer with no waiting.
async fn get_lsm_capture(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
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
async fn get_lsm_capture_aligned(
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
/// curl http://192.168.2.1:8080/api/lsm_control?dc_block=0
/// curl http://192.168.2.1:8080/api/lsm_control?dc_block=1
/// ```
async fn get_lsm_control(
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
            "note": "GET /api/lsm_control?dc_block=0 disables the LSM \
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
/// 3. `?retune_hz=<i64>` -- manually write the traffic DDC NCO offset
///    in Hz, signed, relative to the AD9361 RX LO. Bypasses the
///    grant follower entirely. Does NOT touch `demod_enable` --
///    explicit by design (see #4).
/// 4. `?demod_enable=0|1` -- manually flip the
///    `traffic_demod_control.demod_enable` register bit. Required
///    after a manual retune to actually start the dibit stream.
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
                    let core = state.ip_core.lock().await;
                    // Read the AD9361 sample rate from the cached
                    // register (the same value the startup configure
                    // call used). For Phase 7A.1 we hard-code this
                    // from the well-known default; if we ever start
                    // varying sample rate at runtime this needs to
                    // come from a shared config struct instead.
                    let sample_rate_hz = 8_000_000.0_f64;
                    match core.set_traffic_ddc_frequency(
                        offset_hz as f64,
                        sample_rate_hz,
                    ) {
                        Ok(()) => {
                            applied.push(format!(
                                "retune_hz={offset_hz}"
                            ));
                            // Mirror the manager-side bookkeeping so
                            // /api/traffic shows the new offset
                            // immediately even though the follower
                            // didn't drive it.
                            let mut mgr =
                                state.traffic_manager.lock().await;
                            mgr.last_offset_hz = offset_hz;
                            // Recompute the NCO word the same way
                            // the helper does, so the dashboard's
                            // displayed nco_word matches the register.
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
        "running":         s.started_at.is_some(),
        "uptime_secs":     uptime_secs,
        "last_at_ms_ago":  last_at_ms_ago,
        "total":           s.total,
        "dibit":           s.dibit,
        "traffic":         s.traffic,
        "iq":              s.iq,
        "lsm_dibit":       s.lsm_dibit,
        "rate_per_sec": {
            "total":     rate(s.total),
            "dibit":     rate(s.dibit),
            "traffic":   rate(s.traffic),
            "iq":        rate(s.iq),
            "lsm_dibit": rate(s.lsm_dibit),
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
</div>

<!-- ═════════════════════ Debug tab ═════════════════════ -->
<div class="tab-pane" id="tab-debug">

<!-- ── Phase 9: Decoder Comparison Matrix (3-column PS/PL view) ── -->
<h2>Decoder Comparison (PS framer vs PL gateware)</h2>
<div class="card">
  <table id="cmp_t" style="font-size:0.85em">
    <thead>
      <tr>
        <th style="width:32%">Metric</th>
        <th>PS C4FM<br><span style="color:var(--text-dim);font-weight:400">software, HDL C4FM dibits (dormant on LSM sites)</span></th>
        <th>PS LSM framer<br><span style="color:var(--text-dim);font-weight:400">software framer, PL HDL LSM dibit-fed (production)</span></th>
        <th>PL HDL LSM<br><span style="color:var(--text-dim);font-weight:400">FPGA gateware (heartbeat snapshot)</span></th>
      </tr>
    </thead>
    <tbody id="cmp_body">
      <tr><td colspan="4" style="color:var(--text-dim)">Loading...</td></tr>
    </tbody>
  </table>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
    Three-column view: PS = Processing System (ARM software), PL =
    Programmable Logic (FPGA). Phase 9 (2026-04-15) retired the Phase
    6D pure-software LSM pipeline and its `ps_iq_lsm` + `ps_phase6d`
    columns — the HDL LSM chain is now the production decoder and the
    PS-LSM column is a pass-through framer on top of PL-emitted dibits.
    "(PS only)" marks rows that have no PL equivalent by design
    (PL is a NID decoder, not a TSBK framer). "(= PS)" marks rows
    where the PS column is the authoritative counter for a value
    that's actually generated in the PL. "(HDL: hit-only)" marks the
    sync near-miss row — the HDL hard-sync correlator only fires when
    Hamming distance ≤ threshold, so it doesn't count misses.
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
      <tr><th>Dibit Count <span style="color:var(--text-dim);font-size:0.85em">(C4FM HDL)</span></th><td class="v" id="dibits">0</td></tr>
      <tr><th>Overflow <span style="color:var(--text-dim);font-size:0.85em">(C4FM HDL)</span></th><td class="v" id="overflow">No</td></tr>
    </table>
    <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
      Decoder counters from PS LSM software decoder. Dibit Count + Overflow are
      from the C4FM HDL chain DMA ring (not migrated).
    </p>
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

<h2>Dibit Stream Diagnostics (PS C4FM fallback vs PL HDL LSM)</h2>
<div class="grid2">
  <div class="card">
    <h2>PS C4FM Dibit Stream <span style="font-size:0.75em;color:var(--text-dim);margin-left:6px">c4fm_dibit_dma</span></h2>
    <table>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Histogram</th></tr>
      <tr><th>Total Dibits</th><td class="v" id="dh_total">0</td></tr>
      <tr><th>Value 0 (+1)</th><td class="v" id="dh_0">--</td></tr>
      <tr><th>Value 1 (+3)</th><td class="v" id="dh_1">--</td></tr>
      <tr><th>Value 2 (-1)</th><td class="v" id="dh_2">--</td></tr>
      <tr><th>Value 3 (-3)</th><td class="v" id="dh_3">--</td></tr>
      <tr><th>Inner / Outer ratio</th><td class="v" id="dh_io">--</td></tr>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Sync correlator</th></tr>
      <tr><th>Sync hits</th><td class="v" id="sy_hits">0</td></tr>
      <tr><th>Near misses</th><td class="v" id="sy_near">0</td></tr>
      <tr><th>Best Hamming distance</th><td class="v" id="sy_best">--</td></tr>
      <tr><th colspan="2" style="color:var(--text-dim);text-align:left">Raw on-air DUID histogram</th></tr>
      <tr><th>Total NIDs</th><td class="v" id="rd_total">0</td></tr>
      <tr><th>Bucket 7 TSDU %</th><td class="v" id="rd_7">--</td></tr>
      <tr><th>Bucket 5 LDU1 %</th><td class="v" id="rd_5">--</td></tr>
      <tr><th>Bucket A LDU2 %</th><td class="v" id="rd_a">--</td></tr>
      <tr><th>Bucket 0 HDU %</th><td class="v" id="rd_0">--</td></tr>
    </table>
  </div>
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
      Side-by-side: identical metrics on the C4FM HDL dibit stream vs the
      LSM HDL dibit stream. If histograms differ, the slicers see different
      signal statistics. If sync best distance differs, frame alignment
      between the two streams is diverging. If TSDU bucket % is &lt;90% on
      either, NID payload bits are being corrupted upstream.
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
  <!-- Live control-channel retune. Fires GET /api/reinit with the
       entered frequency (MHz, converted to Hz) and preserves the
       current rf_bandwidth so the v2 8 MHz accept condition stays
       in effect. User can type e.g. "855.4875" to jump to a
       nearby LSM system without editing boot config. -->
  <div style="display:flex;align-items:center;gap:8px;margin-top:8px;font-size:0.85em">
    <label for="bi_retune_mhz"><b>Retune control</b>:</label>
    <input type="number" id="bi_retune_mhz" step="0.001" placeholder="MHz (e.g. 855.4875)"
      style="width:12em;font-family:inherit" />
    <button id="bi_retune_btn" class="btn" style="padding:3px 10px" onclick="retuneControl()">Tune</button>
    <span class="v" id="bi_retune_status" style="font-size:0.85em;color:var(--text-dim)">idle</span>
  </div>
  <p style="color:var(--text-dim);font-size:0.75em;margin-top:6px">
    Pulled from /api/stats every 2 s. Wall clock is the Linux system
    clock — reads as 1970-... until NTP syncs at boot. Audio WS lag is
    the cumulative count of broadcast-channel Lagged events (server
    saw a browser consumer fall behind); non-zero means the listener
    heard a gap. Distinct from the AudioWorklet underrun counter in
    the playback status line below, which is the browser-side ring
    running dry. Retune field takes a control-channel frequency in
    MHz and hits /api/reinit?control_freq=&lt;Hz&gt;&amp;rf_bandwidth=&lt;current&gt;
    — the LO stays at boot value and the DDC NCO shifts to land
    the new channel on the decode path. Board should re-acquire
    within a few seconds. Blank input resets to boot control_freq.
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

// Retune control channel via /api/reinit. Preserves the CURRENT
// rf_bandwidth read from /api/stats so a live bandwidth override
// (e.g. iio_attr-set 8 MHz) doesn't get silently reset back to the
// 4 MHz boot default. Blank input = restore boot control_freq.
async function retuneControl() {
  const mhzStr = $('bi_retune_mhz').value.trim();
  const status = $('bi_retune_status');
  const btn = $('bi_retune_btn');
  btn.disabled = true;
  status.textContent = 'retuning...';
  status.style.color = 'var(--text-dim)';
  try {
    // Grab current rf_bandwidth so we don't drop back to 4 MHz.
    const curStats = await fetchJson('/api/stats');
    const curBw = (curStats && curStats.rf_bandwidth_hz) || 0;
    const params = new URLSearchParams();
    if (mhzStr !== '') {
      const mhz = parseFloat(mhzStr);
      if (!isFinite(mhz) || mhz < 100 || mhz > 6000) {
        status.textContent = 'bad MHz (100-6000 expected)';
        status.style.color = 'var(--red)';
        btn.disabled = false;
        return;
      }
      const hz = Math.round(mhz * 1e6);
      params.set('control_freq', String(hz));
    }
    if (curBw > 0) params.set('rf_bandwidth', String(curBw));
    const url = '/api/reinit' + (params.toString() ? '?' + params.toString() : '');
    const res = await fetchJson(url);
    if (res && res.ok) {
      const freqMhz = mhzStr !== ''
        ? parseFloat(mhzStr).toFixed(4)
        : 'boot default';
      status.textContent = `tuned to ${freqMhz} MHz, waiting for reacquire...`;
      status.style.color = 'var(--green)';
      // Force an immediate refresh so the user sees the new NAC
      // appear as soon as the decoder locks. The 2 s poll will
      // keep updating after that.
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

async function refresh() {
  const sys = await fetchJson('/api/system');
  if (sys) {
    $('nac').textContent = sys.nac || '--';
    $('wacn').textContent = sys.wacn || '--';
    $('sys').textContent = sys.system_id || '--';
    $('rfss').textContent = (sys.rfss_id != null ? `${sys.rfss_id} / ${sys.site_id}` : '--');
    $('cc').textContent = sys.control_channel || '--';
    if (sys.build) $('build_tag').textContent = 'build: ' + sys.build;
  }

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
    const rows = [
      ['NAC (winner)', cmp.ps_c4fm.system_nac, cmp.ps_lsm.system_nac, cmp.pl_hdl.winner_nac],
      ['Messages decoded', fmtN(cmp.ps_c4fm.messages), fmtN(cmp.ps_lsm.messages), '(PS only)'],
      ['Total NIDs', '--', '--', fmtN(pl_total_nids)],
      ['Valid NIDs', '--', '--', fmtN(pl_valid_nids) + ' (' + fmtPct(cmp.pl_hdl.valid_pct) + ')'],
      ['Sync hits (frame sync correlator)', fmtN(cmp.ps_c4fm.sync_hits), fmtN(cmp.ps_lsm.sync_hits), fmtN(pl_total_nids)],
      ['Sync near-misses', fmtN(cmp.ps_c4fm.sync_near), fmtN(cmp.ps_lsm.sync_near), '(HDL: hit-only)'],
      ['Sync best Hamming distance', fmtN(cmp.ps_c4fm.sync_best_dist), fmtN(cmp.ps_lsm.sync_best_dist), fmtN(cmp.pl_hdl.sync_distance)],
      ['Total dibits processed', fmtN(cmp.ps_c4fm.total_dibits), fmtN(cmp.ps_lsm.total_dibits), fmtN(pl_total_dibits) + ' (= PS)'],
      ['Active grants', fmtN(cmp.ps_c4fm.active_grants), fmtN(cmp.ps_lsm.active_grants), '(PS only)'],
      ['Frequency bands known', fmtN(cmp.ps_c4fm.bands_known), fmtN(cmp.ps_lsm.bands_known), '(PS only)'],
      ['Drop count (PL only)', '--', '--', fmtN(cmp.pl_hdl.drop_count)],
      ['Live PLL register', '--', '--', fmtN(cmp.pl_hdl.pll_dbg)],
      ['Live sample-point register', '--', '--', fmtN(cmp.pl_hdl.sp_dbg)],
      ['Overflow events', '--', '--', 'dibit:' + fmtN(cmp.pl_hdl.dibit_overflow_ticks) + ' iq:' + fmtN(cmp.pl_hdl.iq_overflow_ticks)],
      ['── pipeline ──', '', '', ''],
      ['NID attempts (sync hit)', fmtN(cmp.ps_c4fm.nid_attempts), fmtN(cmp.ps_lsm.nid_attempts), fmtN(pl_total_nids)],
      ['NID BCH decode failures', fmtN(cmp.ps_c4fm.nid_decode_failures), fmtN(cmp.ps_lsm.nid_decode_failures), fmtN(pl_nid_fail)],
      ['NID invalid DUID after BCH', fmtN(cmp.ps_c4fm.nid_invalid_duid), fmtN(cmp.ps_lsm.nid_invalid_duid), '(HDL: always valid)'],
      ['NID decoded OK (any DUID)', fmtN(cmp.ps_c4fm.nid_decoded_ok), fmtN(cmp.ps_lsm.nid_decoded_ok), fmtN(pl_valid_nids)],
      ['NID decoded OK (TSDU only)', fmtN(cmp.ps_c4fm.nid_decoded_tsdu), fmtN(cmp.ps_lsm.nid_decoded_tsdu), '(PS only)'],
      ['TSDU attempts', fmtN(cmp.ps_c4fm.tsdu_attempts), fmtN(cmp.ps_lsm.tsdu_attempts), '(PS framer)'],
      ['TSBK block attempts', fmtN(cmp.ps_c4fm.tsbk_block_attempts), fmtN(cmp.ps_lsm.tsbk_block_attempts), '(PS framer)'],
      ['TSBK trellis failures', fmtN(cmp.ps_c4fm.tsbk_trellis_failures), fmtN(cmp.ps_lsm.tsbk_trellis_failures), '(PS framer)'],
      ['TSBK CRC failures', fmtN(cmp.ps_c4fm.tsbk_crc_failures), fmtN(cmp.ps_lsm.tsbk_crc_failures), '(PS framer)'],
      ['TSBK CRC OK', fmtN(cmp.ps_c4fm.tsbk_crc_ok), fmtN(cmp.ps_lsm.tsbk_crc_ok), '(PS framer)'],
      ['  - via plain CRC convention', fmtN(cmp.ps_c4fm.tsbk_crc_ok_plain), fmtN(cmp.ps_lsm.tsbk_crc_ok_plain), '(PS framer)'],
      ['  - via xored 0xFFFF convention', fmtN(cmp.ps_c4fm.tsbk_crc_ok_xored), fmtN(cmp.ps_lsm.tsbk_crc_ok_xored), '(PS framer)'],
      ['TSBK unknown opcode', fmtN(cmp.ps_c4fm.tsbk_unknown_opcode), fmtN(cmp.ps_lsm.tsbk_unknown_opcode), '(PS framer)'],
    ];
    $('cmp_body').innerHTML = rows.map(r =>
      '<tr><th>' + r[0] + '</th>' +
      '<td class="v">' + r[1] + '</td>' +
      '<td class="v">' + r[2] + '</td>' +
      '<td class="v">' + r[3] + '</td></tr>'
    ).join('');
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
      // Reverse so newest is on top.
      const ring = hdl.nid_ring.slice().reverse();
      $('nid_ring_body').innerHTML = ring.map(e =>
        '<tr>' +
        '<td class="v">' + e.seq + '</td>' +
        '<td class="v">' + e.t_ms_since_boot + '</td>' +
        '<td class="v">0x' + e.nac.toString(16).toUpperCase().padStart(3, '0') + '</td>' +
        '<td class="v">' + e.duid + '</td>' +
        '<td class="v" style="color:' + (e.valid ? 'var(--green)' : 'var(--red)') + '">' + (e.valid ? '\u2713' : '\u2717') + '</td>' +
        '<td class="v">' + e.n_errors + '</td>' +
        '<td class="v">' + e.sync_distance + '</td>' +
        '<td class="v">' + e.drop_count + '</td>' +
        '<td class="v">' + e.pll_dbg + '</td>' +
        '<td class="v">' + e.sp_dbg + '</td>' +
        '</tr>'
      ).join('');
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

  const c4fmIds = {total:'dh_total', v0:'dh_0', v1:'dh_1', v2:'dh_2', v3:'dh_3', io:'dh_io',
    hits:'sy_hits', near:'sy_near', best:'sy_best',
    rd_total:'rd_total', rd_7:'rd_7', rd_5:'rd_5', rd_a:'rd_a', rd_0:'rd_0'};
  const lsmIds = {total:'ldh_total', v0:'ldh_0', v1:'ldh_1', v2:'ldh_2', v3:'ldh_3', io:'ldh_io',
    hits:'lsy_hits', near:'lsy_near', best:'lsy_best',
    rd_total:'lrd_total', rd_7:'lrd_7', rd_5:'lrd_5', rd_a:'lrd_a', rd_0:'lrd_0'};
  renderDibitDump(await fetchJson('/api/dibit_dump'), c4fmIds);
  renderDibitDump(await fetchJson('/api/lsm_dibit_dump'), lsmIds);

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
        // going. Do NOT re-prime — that would hold silence for the
        // full PREFILL duration (~540 ms) after every jitter event,
        // which is very audible. Losing individual samples at 8 kHz
        // is inaudible.
        out[i] = 0;
        this.underruns++;
        continue;
      }
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
  ring: null,
  write: 0,
  read: 0,
  available: 0,
  readFrac: 0,
  ratio: 1.0,           // 8000 / ctxRate, set at startAudio
  underruns: 0,
  priming: true,
};
function audioSpnReset() {
  AUDIO_SPN.ring = new Float32Array(AUDIO_SPN.RING_SIZE);
  AUDIO_SPN.write = 0;
  AUDIO_SPN.read = 0;
  AUDIO_SPN.available = 0;
  AUDIO_SPN.readFrac = 0;
  AUDIO_SPN.underruns = 0;
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
      // Absorb as a single silence sample; don't re-prime (see worklet
      // comment). Individual-sample underruns are inaudible at 8 kHz.
      out[i] = 0;
      s.underruns++;
      continue;
    }
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

// Start the 1 s log poller unconditionally -- cheap, keeps the ring
// warm so switching to the Logs tab has instant history.
LOGS.timer = setInterval(logPoll, 1000);
logPoll(); // kick off immediately

loadAliases();
refresh();
setInterval(refresh, 2000);
connectWs();
</script>
</body>
</html>"##;
