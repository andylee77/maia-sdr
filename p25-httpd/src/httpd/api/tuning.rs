//! Runtime knobs: retune, gain, modulation, BCH/sync thresholds, decoder reset.
//!
//! Consumer orientation: "change how the radio behaves without a
//! reboot." Every endpoint here mutates state that would otherwise
//! require editing `main.rs` + reflashing. If a new HDL parameter or
//! AD9361 field needs runtime tuning, this is the home — matches the
//! "no private backchannels" rule in `doc/API_CONSUMERS.md`.
//!
//! Not a user-facing screen in the usual sense — these are advanced
//! controls an Android "expert mode" panel would expose. Most casual
//! consumers stay on `api::radio`.
//!
//! Write semantics: every GET is read-only; writes happen via
//! optional query params on GET (legacy pattern, e.g. `?dc_block=1`)
//! or via PUT. Both are documented in the endpoint catalogue.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};

#[allow(unused_imports)]
use p25_json::*;

#[allow(unused_imports)]
use crate::hardware::ddc_presets::{self, DdcPreset};
#[allow(unused_imports)]
use crate::httpd::AppState;
#[allow(unused_imports)]
use crate::protocol::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, CC_SYNC_THRESHOLD,
};

/// Scanner guard band. `/api/tune` in Auto mode recenters the LO
/// whenever the requested radio frequency falls within
/// `(BW/2 − GUARD_HZ)` of either window edge. Keeps the NCO well
/// clear of the DDC stage-1 transition band.
const TUNE_GUARD_HZ: i64 = 100_000;
/// In Auto-recenter, round the new LO to this step so the AD9361
/// PLL doesn't re-settle on every kHz of operator scrolling.
const TUNE_LO_STEP_HZ: i64 = 100_000;

// ── Tuning API (2026-04-22 redesign) ──────────────────────────────
//
// Three endpoints replace the old `/api/reinit`:
//
//   GET  /api/presets     — list every DDC preset + its spec.
//   POST /api/preset      — apply a preset (slow path: AD9361 resettle
//                            + DDC coefficient reload). Optional
//                            center frequency + gain override.
//   POST /api/tune        — move the radio frequency (fast path:
//                            NCO-only) or auto-recenter the LO when
//                            the window would be exceeded. Lock mode
//                            refuses LO moves and returns 409 instead.
//
// The two POST endpoints are JSON-in / JSON-out; the GET is read-only.


/// `GET /api/presets` — enumerate every DDC preset available at
/// runtime. The current live preset is identified by
/// `current_preset.name`. The dashboard reads this once at page load
/// to populate the sample-rate / BW dropdown.
pub async fn get_presets(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;
    let cur_idx = state.current_preset_idx.load(Ordering::Relaxed);
    let cur = ddc_presets::PRESETS
        .get(cur_idx)
        .copied()
        .unwrap_or(ddc_presets::DEFAULT_PRESET);

    let list: Vec<serde_json::Value> = ddc_presets::PRESETS
        .iter()
        .map(|p| {
            serde_json::json!({
                "name":             p.name,
                "sample_rate_hz":   p.sample_rate_hz,
                "rf_bandwidth_hz":  p.rf_bandwidth_hz,
                "total_decim":      p.total_decim(),
                "decim":            [p.decim1, p.decim2, p.decim3],
                "nco_half_window_hz": p.nco_half_window_hz(),
                "rejection_25k_db": p.rejection_25k_db,
            })
        })
        .collect();

    Json(serde_json::json!({
        "presets":        list,
        "current":        cur.name,
        "default":        ddc_presets::DEFAULT_PRESET.name,
        "center_locked":  state.center_locked.load(Ordering::Relaxed),
        "note":           "Every preset produces 62.5 kSPS at the DDC \
                           output by construction. Preset choice controls \
                           AD9361 sample rate + RF bandwidth + the NCO \
                           window width (= sample_rate/2).",
    }))
}


/// `POST /api/preset` body — applied by `post_preset`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetBody {
    /// Preset name (e.g. "8M"). Must exist in `PRESETS`.
    pub preset: String,
    /// Optional center (RX LO) override in Hz. If omitted, the live
    /// RX LO is preserved across the preset change.
    #[serde(default)]
    pub center_freq_hz: Option<u64>,
    /// Optional gain-mode override ("manual", "slow_attack",
    /// "fast_attack", "hybrid"). Omitted = no change.
    #[serde(default)]
    pub gain_mode: Option<String>,
    /// Manual gain in dB, only meaningful when gain_mode == "manual".
    #[serde(default)]
    pub gain_db: Option<f64>,
}


/// `POST /api/preset` — slow path: resettle the AD9361 + reload DDC
/// coefficients for a new sample-rate / BW preset. The control
/// channel is retuned to `boot_control_freq` (no scanner semantics
/// on a preset change — the operator is explicitly picking a new
/// front-end state, so we re-center on the known-good site freq).
#[cfg(target_os = "linux")]
pub async fn post_preset(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PresetBody>,
) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    let Some(preset) = ddc_presets::find_preset(&body.preset) else {
        let names: Vec<&str> = ddc_presets::PRESETS
            .iter().map(|p| p.name).collect();
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "ok":    false,
            "error": format!(
                "unknown preset '{}'; known: {}",
                body.preset, names.join(", "),
            ),
        })));
    };
    let preset_idx = ddc_presets::PRESETS
        .iter().position(|p| p.name == preset.name).unwrap();

    let new_rx_lo = body.center_freq_hz.unwrap_or_else(||
        state.current_rx_lo.load(Ordering::Relaxed) as u64);

    let gain_mode = match body.gain_mode.as_deref() {
        None => None,
        Some("manual") => Some(crate::hardware::iio::GainMode::Manual),
        Some("slow_attack") => Some(
            crate::hardware::iio::GainMode::SlowAttack),
        Some("fast_attack") => Some(
            crate::hardware::iio::GainMode::FastAttack),
        Some("hybrid") => Some(crate::hardware::iio::GainMode::Hybrid),
        Some(other) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok":    false,
                "error": format!(
                    "unknown gain_mode '{other}'; expected \
                     manual|slow_attack|fast_attack|hybrid"
                ),
            })));
        }
    };

    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // 1. AD9361 — LO, sample rate, RF bandwidth, optional gain mode +
    //    manual gain. Order matters: change gain_mode BEFORE writing
    //    gain_db so the mode change doesn't clobber the new value.
    match state.ad9361.set_rx_lo_frequency(new_rx_lo).await {
        Ok(_) => {
            state.current_rx_lo.store(
                new_rx_lo as i64, Ordering::Relaxed);
            applied.push(format!("rx_lo={new_rx_lo}"));
        }
        Err(e) => errors.push(format!("rx_lo: {e}")),
    }
    match state.ad9361
        .set_sampling_frequency(preset.sample_rate_hz).await
    {
        Ok(_) => {
            state.current_sample_rate_hz.store(
                preset.sample_rate_hz, Ordering::Relaxed);
            applied.push(format!(
                "sampling_frequency={}", preset.sample_rate_hz));
        }
        Err(e) => errors.push(format!("sampling_frequency: {e}")),
    }
    match state.ad9361
        .set_rx_rf_bandwidth(preset.rf_bandwidth_hz).await
    {
        Ok(_) => applied.push(format!(
            "rf_bandwidth={}", preset.rf_bandwidth_hz)),
        Err(e) => errors.push(format!("rf_bandwidth: {e}")),
    }
    if let Some(gm) = gain_mode {
        match state.ad9361.set_rx_gain_mode(gm).await {
            Ok(_) => applied.push(format!("gain_mode={gm}")),
            Err(e) => errors.push(format!("gain_mode: {e}")),
        }
        if matches!(gm, crate::hardware::iio::GainMode::Manual) {
            if let Some(db) = body.gain_db {
                match state.ad9361.set_rx_gain(db).await {
                    Ok(_) => applied.push(format!("gain_db={db}")),
                    Err(e) => errors.push(format!("gain_db: {e}")),
                }
            }
        }
    }

    // 2. DDC — load new FIR coefficients + decimation + NCO offset
    //    for the current control-channel frequency. Same NCO math as
    //    boot (PPM-corrected).
    let nco_lo_shift_hz = -state.boot_lo_ppm * 1e-6 * new_rx_lo as f64;
    let nco_offset_hz = state.boot_control_freq as f64
        - new_rx_lo as f64 + nco_lo_shift_hz;
    {
        let core = state.ip_core.lock().await;
        match core.configure_ddc(nco_offset_hz, preset) {
            Ok(_) => {
                state.current_preset_idx.store(
                    preset_idx, Ordering::Relaxed);
                applied.push(format!(
                    "configure_ddc preset={} nco_offset_hz={:.0}",
                    preset.name, nco_offset_hz));
            }
            Err(e) => errors.push(format!("configure_ddc: {e}")),
        }
    }

    let readback_gain = state.ad9361.get_rx_gain().await.ok();
    let readback_rssi = state.ad9361.get_rx_rssi().await.ok();

    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(serde_json::json!({
        "ok":       errors.is_empty(),
        "applied":  applied,
        "errors":   errors,
        "preset":   preset.name,
        "sample_rate_hz":  preset.sample_rate_hz,
        "rf_bandwidth_hz": preset.rf_bandwidth_hz,
        "rx_lo_hz":        new_rx_lo,
        "nco_offset_hz":   nco_offset_hz,
        "readback": {
            "hardwaregain_db": readback_gain,
            "rssi_db":         readback_rssi,
        },
    })))
}


#[cfg(not(target_os = "linux"))]
pub async fn post_preset(
    State(_state): State<Arc<AppState>>,
    Json(_body): Json<PresetBody>,
) -> impl IntoResponse {
    (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
        "ok":    false,
        "error": "preset apply is only available on the target (linux/arm)",
    })))
}


/// `POST /api/tune` body — applied by `post_tune`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TuneBody {
    /// Target radio (channel) frequency in Hz.
    pub radio_freq_hz: u64,
    /// Optional center-mode override. When omitted, uses the live
    /// `center_locked` flag. "auto" = may move the LO. "lock" = NCO
    /// only; 409 if outside window.
    #[serde(default)]
    pub center_mode: Option<String>,
}


/// `POST /api/tune` — scanner-style retune. In Auto mode the LO
/// moves only when the NCO window would be exceeded, so stepping
/// inside the current BW is a pure register write (fast). In Lock
/// mode the LO never moves; out-of-window requests return 409.
#[cfg(target_os = "linux")]
pub async fn post_tune(
    State(state): State<Arc<AppState>>,
    Json(body): Json<TuneBody>,
) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    let lock_req = match body.center_mode.as_deref() {
        None => state.center_locked.load(Ordering::Relaxed),
        Some("auto") => false,
        Some("lock") => true,
        Some(other) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok":    false,
                "error": format!(
                    "unknown center_mode '{other}'; expected 'auto' or 'lock'"
                ),
            })));
        }
    };

    let preset_idx = state.current_preset_idx.load(Ordering::Relaxed);
    let preset = ddc_presets::PRESETS
        .get(preset_idx)
        .copied()
        .unwrap_or(ddc_presets::DEFAULT_PRESET);
    let sample_rate_hz = preset.sample_rate_hz as f64;
    let rx_lo_now: i64 = state.current_rx_lo.load(Ordering::Relaxed);
    let radio: i64 = body.radio_freq_hz as i64;
    let half_window: i64 = preset.nco_half_window_hz() as i64;
    let usable_half: i64 = half_window - TUNE_GUARD_HZ;

    // Compute the NCO offset at the existing LO. If it fits the
    // guard-banded window we can do a pure NCO move regardless of
    // mode. Otherwise Lock returns 409, Auto recenters.
    let offset_at_current_lo: i64 = radio - rx_lo_now;
    let in_window = offset_at_current_lo.abs() <= usable_half;

    let (new_rx_lo, lo_moved) = if in_window {
        (rx_lo_now, false)
    } else if lock_req {
        return (StatusCode::CONFLICT, Json(serde_json::json!({
            "ok":    false,
            "error": "radio frequency outside locked window",
            "radio_freq_hz":     radio,
            "rx_lo_hz":          rx_lo_now,
            "window_half_hz":    usable_half,
            "preset":            preset.name,
            "hint":              "Unlock center (POST /api/tune body \
                                  center_mode=\"auto\") or pick a preset \
                                  with a wider NCO window.",
        })));
    } else {
        // Auto recenter: round the radio frequency to the nearest
        // TUNE_LO_STEP_HZ multiple. That lands the NCO at ~0 Hz for
        // a typical grid frequency and keeps the AD9361 PLL from
        // resettling on sub-step scrolls.
        let rounded = ((radio + TUNE_LO_STEP_HZ / 2) / TUNE_LO_STEP_HZ)
            * TUNE_LO_STEP_HZ;
        (rounded, true)
    };

    let nco_lo_shift_hz = -state.boot_lo_ppm * 1e-6 * new_rx_lo as f64;
    let nco_offset_hz = radio as f64 - new_rx_lo as f64 + nco_lo_shift_hz;

    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    if lo_moved {
        match state.ad9361.set_rx_lo_frequency(new_rx_lo as u64).await {
            Ok(_) => {
                state.current_rx_lo.store(new_rx_lo, Ordering::Relaxed);
                applied.push(format!("rx_lo={new_rx_lo}"));
            }
            Err(e) => errors.push(format!("rx_lo: {e}")),
        }
    }
    {
        let core = state.ip_core.lock().await;
        match core.set_ddc_frequency(nco_offset_hz, sample_rate_hz) {
            Ok(_) => applied.push(format!(
                "ddc_nco_offset={:.0}", nco_offset_hz)),
            Err(e) => errors.push(format!("ddc_nco_offset: {e}")),
        }
    }
    // Reflect the lock state requested by this call.
    state.center_locked.store(lock_req, Ordering::Relaxed);

    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(serde_json::json!({
        "ok":              errors.is_empty(),
        "applied":         applied,
        "errors":          errors,
        "radio_freq_hz":   radio,
        "rx_lo_hz":        new_rx_lo,
        "nco_offset_hz":   nco_offset_hz,
        "lo_moved":        lo_moved,
        "center_locked":   lock_req,
        "preset":          preset.name,
        "window_half_hz":  usable_half,
    })))
}


#[cfg(not(target_os = "linux"))]
pub async fn post_tune(
    State(_state): State<Arc<AppState>>,
    Json(_body): Json<TuneBody>,
) -> impl IntoResponse {
    (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({
        "ok":    false,
        "error": "tune is only available on the target (linux/arm)",
    })))
}


/// `GET /api/modulation` — returns current mode + NID-valid rates
/// for both decoders so the dashboard can show why auto-detect
/// picked what it did.
pub async fn get_modulation(
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
pub async fn put_modulation(
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


/// Phase 6F.7: read the current runtime sync threshold + a quick
/// histogram-based "tuning hint" so an operator can decide what to
/// set next without rebuilding the dashboard.
///
/// **Dual-mode endpoint:** if called with `?threshold=N`, this also
/// updates the runtime threshold (mirroring the PUT handler) so it
/// works from a browser bar or plain `curl` without `-X PUT`. The
/// response always contains the *current* (post-update) value.
pub async fn get_sync_tune(
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
        "default_threshold":   CC_SYNC_THRESHOLD,
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
pub async fn get_decoder_reset(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
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


pub async fn post_decoder_reset(state: State<Arc<AppState>>) -> Json<serde_json::Value> {
    get_decoder_reset(state).await
}


/// Phase 10-prep: GET /api/rx_gain -- AD9361 RX gain + AGC mode knob.
///
/// Read-only without params. Writeable params (both optional, can be
/// combined):
///   - `?mode=manual|slow_attack|fast_attack|hybrid` — sets
///     `in_voltage0_gain_control_mode`. `slow_attack` is the standard
///     AGC; the other modes are AD9361-specific options mostly useful
///     for bursty traffic (fast_attack) or experimentation (hybrid).
///   - `?db=<int>` — sets `in_voltage0_hardwaregain` via IIO. Range
///     [-3, 76] dB in 1 dB steps. Only takes effect when the mode is
///     `manual` — the AD9361 ignores writes in AGC modes — so if both
///     params are present, the mode change is applied first.
///
/// Response: current gain_db, mode, rssi_db, updated_from, error,
/// valid ranges. Matches the pre-mode-support shape plus `mode_from`.
pub async fn get_rx_gain(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    #[cfg(target_os = "linux")]
    {
        use crate::hardware::iio::GainMode;

        let mut updated_from: Option<i64> = None;
        let mut mode_from: Option<String> = None;
        let mut error: Option<String> = None;

        // Mode first so a combined "switch to manual + set gain" call
        // works in one request. Writes to hardwaregain in an AGC mode
        // are silent no-ops from the AD9361 side.
        if let Some(v) = params.get("mode") {
            match v.parse::<GainMode>() {
                Ok(new_mode) => {
                    let prev = state.ad9361.get_rx_gain_mode().await.ok()
                        .map(|m| m.to_string());
                    match state.ad9361.set_rx_gain_mode(new_mode).await {
                        Ok(()) => {
                            mode_from = prev.clone();
                            state.event_log.push(
                                crate::services::event_log::LogCategory::System,
                                format!("gain_control_mode set to {new_mode}"),
                                serde_json::json!({
                                    "mode": new_mode.to_string(),
                                    "previous": prev,
                                }),
                            );
                        }
                        Err(e) => error = Some(format!("set_rx_gain_mode: {e}")),
                    }
                }
                Err(_) => error = Some(format!(
                    "mode '{v}' invalid; expected manual|slow_attack|fast_attack|hybrid"
                )),
            }
        }

        if error.is_none() {
            if let Some(v) = params.get("db") {
                match v.parse::<i64>() {
                    Ok(db) if (-3..=76).contains(&db) => {
                        let prev = state.ad9361.get_rx_gain().await.ok()
                            .map(|f: f64| f as i64);
                        match state.ad9361.set_rx_gain(db as f64).await {
                            Ok(()) => {
                                updated_from = prev;
                                state.event_log.push(
                                    crate::services::event_log::LogCategory::System,
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
            "mode_from":     mode_from,
            "error":         error,
            "range_db":      [-3, 76],
            "valid_modes":   ["manual", "slow_attack", "fast_attack", "hybrid"],
            "note": "GET /api/rx_gain?mode=<m>&db=<N> — mode switches \
                     gain_control_mode; db sets manual hardwaregain \
                     (only effective in manual mode). Either param can \
                     be omitted.",
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
pub async fn put_rx_gain(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    get_rx_gain(State(state), axum::extract::Query(params)).await
}


/// Phase 6F.7: PUT /api/sync_tune?threshold=N -- update the runtime
/// sync threshold without rebuilding. Validates 0 <= N <= 24.
pub async fn put_sync_tune(
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
                crate::services::event_log::LogCategory::System,
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
                crate::services::event_log::LogCategory::System,
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
                "default":       CC_SYNC_THRESHOLD,
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


/// GET /api/bch_t -- read current BCH-t override for both decoder sides.
pub async fn get_bch_t(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let control = state.lsm_decoder.read().await.bch_t_override;
    let traffic = state.traffic_lsm_decoder.read().await.bch_t_override;
    Json(serde_json::json!({
        "control":  control,
        "traffic":  traffic,
        "default":  crate::protocol::p25::fec::bch::T_MAX_ERRORS,
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
pub async fn put_bch_t(
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
            Ok(v) if v <= crate::protocol::p25::fec::bch::T_MAX_ERRORS => Some(v),
            Ok(v) => {
                return Json(serde_json::json!({
                    "error": format!(
                        "value={} out of range (max = {})",
                        v, crate::protocol::p25::fec::bch::T_MAX_ERRORS,
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
        crate::services::event_log::LogCategory::System,
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


