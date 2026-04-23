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
    //    for the current control-channel frequency. Uses the live
    //    crystal-trim correction (`current_lo_shift_hz`) so any
    //    auto-PPM run survives a preset reload.
    let nco_lo_shift_hz = state.current_lo_shift_hz
        .load(Ordering::Relaxed) as f64;
    let current_radio = state.current_control_freq
        .load(Ordering::Relaxed) as f64;
    let nco_offset_hz = current_radio
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
    /// Optional explicit LO (center) frequency in Hz. When set, the
    /// AD9361 LO is commanded to this value directly and the NCO is
    /// programmed as `radio_freq_hz - center_hz + lo_shift`. Ignores
    /// the window/guard check — the operator is in charge when they
    /// pick a center. Matches scanner UIs that have a "center freq"
    /// field separate from the channel-select dial.
    #[serde(default)]
    pub center_hz: Option<u64>,
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

    let (new_rx_lo, lo_moved) = if let Some(center) = body.center_hz {
        // Explicit center-freq command. Bypass window checks — the
        // operator is telling us exactly where the LO should sit.
        let c = center as i64;
        if c != rx_lo_now { (c, true) } else { (rx_lo_now, false) }
    } else if in_window {
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

    // Use the live PPM-correction shift (auto-PPM results survive
    // retunes). On a fresh boot this equals the CLI-derived value;
    // after /api/ppm_calibrate it tracks the calibration.
    let nco_lo_shift_hz = state.current_lo_shift_hz
        .load(Ordering::Relaxed) as f64;
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
    // Record the operator-facing radio frequency so /api/stats +
    // dashboard can show what we're actually tuned to. Only commit
    // on DDC success.
    if errors.is_empty() {
        state.current_control_freq.store(
            radio as u64, Ordering::Relaxed);
    }

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

// ── Auto-PPM (2026-04-23) ─────────────────────────────────────────
//
// One-shot wideband FFT peak-find + PLL residual, applied to the
// control DDC NCO. Implementation in `app::autoppm`; this module
// just hosts the HTTP surface.

/// `GET /api/ppm` — current crystal-trim correction state.
///
/// Returns the live DDC NCO shift in Hz and equivalent ppm at the
/// current RX LO, plus the timestamp of the last successful
/// `POST /api/ppm_calibrate` (0 if never calibrated this session —
/// the value shown is then the boot-time `--lo-ppm` value).
pub async fn get_ppm(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;
    let lo_shift_hz = state.current_lo_shift_hz
        .load(Ordering::Relaxed) as f64;
    let rx_lo_hz = state.current_rx_lo
        .load(Ordering::Relaxed) as f64;
    let ppm = if rx_lo_hz > 0.0 {
        -lo_shift_hz / (rx_lo_hz * 1e-6)
    } else { 0.0 };
    let last_cal = state.last_ppm_cal_unix_secs
        .load(Ordering::Relaxed);
    Json(serde_json::json!({
        "ok":                     true,
        "lo_shift_hz":            lo_shift_hz,
        "lo_ppm":                 ppm,
        "rx_lo_hz":               rx_lo_hz,
        "boot_lo_ppm":            state.boot_lo_ppm,
        "last_cal_unix_secs":     last_cal,
        "calibrated_this_session": last_cal != 0,
    }))
}

/// `POST /api/ppm_calibrate` — run one auto-PPM pass (Linux only).
///
/// Grabs one wideband FFT frame, locates the control-channel peak in
/// a ±10 kHz window around the expected offset, applies the delta to
/// the DDC NCO, waits ~3 s for the PLL to re-settle, samples
/// `pll_dbg` ~30 × over 3 s, converts the mean to Hz, and applies the
/// residual. Total runtime ~7 s. Result JSON includes every
/// intermediate so operators can sanity-check.
#[cfg(target_os = "linux")]
pub async fn post_ppm_calibrate(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    match crate::app::autoppm::run_calibration(&state).await {
        Ok(result) => {
            let body = serde_json::json!({
                "ok":     true,
                "result": result,
            });
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(e) => {
            let body = serde_json::json!({
                "ok":    false,
                "error": e.to_string(),
            });
            (StatusCode::INTERNAL_SERVER_ERROR, Json(body))
                .into_response()
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub async fn post_ppm_calibrate(
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let body = serde_json::json!({
        "ok": false,
        "error": "auto-PPM requires the target (Linux/ARM) — this build has no hardware",
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

/// `PUT /api/ppm?lo_shift_hz=<i64>`  — manual override.
///
/// Writes the DDC NCO crystal-trim shift directly without running
/// auto-PPM. Intended for the rare "auto-PPM is broken but I know
/// the right value" case (e.g. wideband FFT disabled, or a stale
/// persisted file rejected on boot). The shift is applied to the
/// live DDC and persisted to `/mnt/jffs2/p25-ppm-cal.json`.
///
/// Accepted range: ±1000 Hz at rx_lo (≈±1.2 ppm at 858 MHz). The
/// usual AD9361 crystal range. Larger values are likely mistakes.
#[cfg(target_os = "linux")]
pub async fn put_ppm(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params):
        axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use std::sync::atomic::Ordering;

    let shift_hz: i64 = match params.get("lo_shift_hz")
        .and_then(|v| v.parse().ok()) {
        Some(v) => v,
        None => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false,
                "error": "missing or invalid 'lo_shift_hz' (i64)",
            }))).into_response();
        }
    };
    if shift_hz.unsigned_abs() > 1000 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "ok": false,
            "error": format!("lo_shift_hz={shift_hz} out of range (|x| <= 1000); \
                             typical AD9361 crystal trim is <~500 Hz at 1 GHz"),
        }))).into_response();
    }

    // Apply to AppState so subsequent /api/tune calls pick it up.
    // A manual override sets the baseline — fine-tune will then
    // track ±0.2 ppm off this operator-chosen value.
    state.current_lo_shift_hz.store(shift_hz, Ordering::Relaxed);
    state.baseline_lo_shift_hz.store(shift_hz, Ordering::Relaxed);

    // Reprogram the DDC NCO NOW so decode recovers without a retune.
    let rx_lo = state.current_rx_lo.load(Ordering::Relaxed) as f64;
    let sample_rate = state.current_sample_rate_hz
        .load(Ordering::Relaxed) as f64;
    let radio_freq = state.current_control_freq
        .load(Ordering::Relaxed) as f64;
    let nco_offset = radio_freq - rx_lo + (shift_hz as f64);
    {
        let core = state.ip_core.lock().await;
        if let Err(e) = core.set_ddc_frequency(nco_offset, sample_rate) {
            return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                "ok": false,
                "error": format!("set_ddc_frequency: {e}"),
            }))).into_response();
        }
    }
    state.last_ppm_cal_unix_secs.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64).unwrap_or(0),
        Ordering::Relaxed);

    let lo_ppm = if rx_lo > 0.0 {
        -(shift_hz as f64) / (rx_lo * 1e-6)
    } else { 0.0 };

    // Event-log the override so operators can audit PPM changes via
    // /api/log, not just via tracing output.
    state.event_log.push(
        crate::services::event_log::LogCategory::System,
        format!("manual PPM override: lo_shift_hz={shift_hz:+} \
                 ({lo_ppm:+.4} ppm, baseline reset)"),
        serde_json::json!({
            "kind":        "ppm.override",
            "lo_shift_hz": shift_hz,
            "lo_ppm":      lo_ppm,
            "nco_offset":  nco_offset,
        }),
    );

    (StatusCode::OK, Json(serde_json::json!({
        "ok":          true,
        "lo_shift_hz": shift_hz,
        "lo_ppm":      lo_ppm,
        "nco_offset":  nco_offset,
        "note":        "shift applied live; persistence to /mnt/jffs2 \
                        will happen on next auto-PPM run or manual cal",
    }))).into_response()
}

#[cfg(not(target_os = "linux"))]
pub async fn put_ppm(
    State(_state): State<Arc<AppState>>,
    axum::extract::Query(_p):
        axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let body = serde_json::json!({
        "ok": false,
        "error": "manual PPM override requires hardware (target_os=linux)",
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

// ── HDL LSM AGC idle-gate threshold knob (2026-04-23) ──────────
//
// Exposes the per-chain `mag_update_threshold` register (Q1.15 raw,
// 16-bit unsigned). Default 256 = -42 dBFS; below this magnitude
// the AGC treats the sample as noise and skips the gain update.
// Tunable per site/antenna from the PS — no HDL rebuild needed.

/// `GET /api/agc_threshold[?chain=control|traffic]`
///
/// Returns the current threshold for both chains (`control_hz` /
/// `traffic_hz`) plus Q1.15 float equivalents. `?chain=` param is
/// accepted for symmetry with `/api/traffic` but doesn't filter the
/// response — both chains are always reported in one call so you
/// can diff them.
#[cfg(target_os = "linux")]
pub async fn get_agc_threshold(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let core = state.ip_core.lock().await;
    let ctrl = core.lsm_agc_threshold();
    let trf  = core.traffic_lsm_agc_threshold();
    Json(serde_json::json!({
        "ok":              true,
        "control":         ctrl,
        "control_f":       (ctrl as f64) / 32768.0,
        "traffic":         trf,
        "traffic_f":       (trf as f64) / 32768.0,
        "valid_range":     [0, 65535],
        "default":         256,
        "note":            "Q1.15 raw; 256 = -42 dBFS; 0 disables gate",
    }))
}

/// `PUT /api/agc_threshold?chain=control|traffic&value=<u16>`
///
/// Writes the threshold for the selected chain. `?chain=both`
/// sets both at once.
#[cfg(target_os = "linux")]
pub async fn put_agc_threshold(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params):
        axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let chain = params.get("chain")
        .map(String::as_str).unwrap_or("both");
    let value: u32 = match params.get("value")
        .and_then(|v| v.parse().ok()) {
        Some(v) => v,
        None => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false,
                "error": "missing or invalid 'value' (u16 0..65535)",
            }))).into_response();
        }
    };
    if value > 0xFFFF {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "ok": false,
            "error": format!("value {value} out of range [0, 65535]"),
        }))).into_response();
    }
    let v = value as u16;
    let core = state.ip_core.lock().await;
    let (applied_ctrl, applied_trf) = match chain {
        "control" => { core.set_lsm_agc_threshold(v); (true, false) }
        "traffic" => { core.set_traffic_lsm_agc_threshold(v); (false, true) }
        "both" => {
            core.set_lsm_agc_threshold(v);
            core.set_traffic_lsm_agc_threshold(v);
            (true, true)
        }
        other => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false,
                "error": format!("unknown chain '{other}'; expected control|traffic|both"),
            }))).into_response();
        }
    };
    let ctrl_now = core.lsm_agc_threshold();
    let trf_now  = core.traffic_lsm_agc_threshold();
    (StatusCode::OK, Json(serde_json::json!({
        "ok":               true,
        "applied_control":  applied_ctrl,
        "applied_traffic":  applied_trf,
        "control":          ctrl_now,
        "traffic":          trf_now,
    }))).into_response()
}

#[cfg(not(target_os = "linux"))]
pub async fn get_agc_threshold(
    State(_state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "agc_threshold requires hardware (target_os=linux)",
    }))
}

#[cfg(not(target_os = "linux"))]
pub async fn put_agc_threshold(
    State(_state): State<Arc<AppState>>,
    axum::extract::Query(_params):
        axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let body = serde_json::json!({
        "ok": false,
        "error": "agc_threshold requires hardware (target_os=linux)",
    });
    (StatusCode::SERVICE_UNAVAILABLE, Json(body)).into_response()
}

