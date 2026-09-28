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
        "note":           "Every preset produces 50 kSPS at the DDC \
                           output by construction. Preset choice controls \
                           AD9361 sample rate + RF bandwidth + the NCO \
                           window width (= sample_rate/2).",
    }))
}


/// `POST /api/preset` body — applied by `post_preset`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetBody {
    /// Preset name (e.g. "8M"). Must exist in `PRESETS`. Change 070:
    /// "auto" = the preset and LO the window planner picks for the
    /// active site (`services::lo_plan`).
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
    if let Some(busy) = state.radio_busy() {
        return busy;
    }
    let (status, reply) = apply_preset(&state, body).await;
    (status, Json(reply))
}

/// Change 070: the body of `POST /api/preset`, shared with the site
/// switch and the recentre task (`app::recentre_task`).
#[cfg(target_os = "linux")]
pub async fn apply_preset(state: &AppState, mut body: PresetBody) -> (StatusCode, serde_json::Value) {
    use std::sync::atomic::Ordering;
    use crate::services::lo_plan;

    // Change 070: the site's channels (listed + granted) for the planner.
    let site_channels = state.active_site.read().await.as_ref().map(|s| {
        lo_plan::channels(&s.traffic_freqs_hz, &state.lo_plans.get().grants)
    });
    let plan_cc = state.current_control_freq.load(Ordering::Relaxed);
    let plan_shift = state.current_lo_shift_hz.load(Ordering::Relaxed);
    if body.preset == "auto" {
        let best = site_channels.as_ref().and_then(|ch| {
            let min = state.lo_plans.get().min_preset;
            lo_plan::plan(plan_cc, ch, &crate::app::recentre_task::plan_presets(min.as_deref()))
        });
        let Some(best) = best else {
            return (StatusCode::CONFLICT, serde_json::json!({
                "ok": false, "error": "preset auto: no active site to plan for",
            }));
        };
        body.preset = best.preset.clone();
        if body.center_freq_hz.is_none() {
            body.center_freq_hz = Some((best.lo_hz + plan_shift) as u64);
        }
    }

    let Some(preset) = ddc_presets::find_preset(&body.preset) else {
        let names: Vec<&str> = ddc_presets::PRESETS
            .iter().map(|p| p.name).collect();
        return (StatusCode::BAD_REQUEST, serde_json::json!({
            "ok":    false,
            "error": format!(
                "unknown preset '{}'; known: {}, auto",
                body.preset, names.join(", "),
            ),
        }));
    };
    let preset_idx = ddc_presets::PRESETS
        .iter().position(|p| p.name == preset.name).unwrap();

    // 2026-05-03: auto-snap the LO when the requested preset's IF
    // window is too narrow to keep the active CC in range, OR when
    // the active site has a `cc_position` that places the CC away
    // from the IF centre (e.g. Clay: traffic 852-861 below CC, snap
    // CC to top of window). An explicit `center_freq_hz` in the
    // request body still overrides (caller knows best).
    let nco_lo_shift_hz = state.current_lo_shift_hz
        .load(Ordering::Relaxed) as f64;
    let current_radio = state.current_control_freq
        .load(Ordering::Relaxed) as f64;
    let half_sr = preset.sample_rate_hz as f64 / 2.0;
    // Active-site cc_position drives the snap target; default
    // Center if no site is loaded (legacy behaviour).
    let cc_position = state
        .active_site
        .read()
        .await
        .as_ref()
        .map(|s| s.cc_position)
        .unwrap_or(crate::services::sites::CcPosition::Center);
    // Margin between CC and IF window edge (Top / Bottom only).
    // 250 kHz keeps the CC clear of the AD9361 transition band.
    const LO_SNAP_MARGIN_HZ: f64 = 250_000.0;
    // Change 070: a site with known channels gets the planner's window
    // for this preset (control channel inside, most channels covered).
    let planned = site_channels
        .filter(|ch| !ch.is_empty())
        .map(|ch| lo_plan::place(plan_cc, &ch, preset.sample_rate_hz));
    let new_rx_lo = if let Some(req_lo) = body.center_freq_hz {
        req_lo
    } else if let Some(p) = planned {
        tracing::info!(
            target: "p25_preset",
            "LO planned for {} preset: {} (channel weight {:.0})",
            preset.name, p.lo_hz + plan_shift, p.covered_weight,
        );
        (p.lo_hz + plan_shift) as u64
    } else {
        // Compute the snap target for this site's cc_position. NCO
        // sees `(cc - lo)`; we want it to land at +offset where:
        //   Top    → +(half_sr - margin)  [CC near top of IF]
        //   Center →  0                    [CC at IF centre]
        //   Bottom → -(half_sr - margin)  [CC near bottom of IF]
        // → lo = cc - offset (+ ppm correction).
        let target_offset = match cc_position {
            crate::services::sites::CcPosition::Top => {
                half_sr - LO_SNAP_MARGIN_HZ
            }
            crate::services::sites::CcPosition::Center => 0.0,
            crate::services::sites::CcPosition::Bottom => {
                -(half_sr - LO_SNAP_MARGIN_HZ)
            }
        };
        let snapped = current_radio - target_offset + nco_lo_shift_hz;
        let prev_lo = state.current_rx_lo.load(Ordering::Relaxed) as f64;
        let raw_nco_at_prev = current_radio - prev_lo + nco_lo_shift_hz;
        // Always snap if cc_position is non-Center (the operator
        // selected a site whose traffic spread asks for it). For
        // Center, keep the legacy behaviour: only snap if the
        // current LO would put the NCO out of window.
        let must_snap = !matches!(
            cc_position,
            crate::services::sites::CcPosition::Center,
        ) || raw_nco_at_prev.abs() > half_sr;
        if must_snap {
            tracing::info!(
                target: "p25_preset",
                "LO snap (cc_position={cc_position:?}): prev_lo={} \
                 raw_nco={:+.0} Hz @ {} preset → snapping to {} \
                 (target offset {:+.0} Hz)",
                prev_lo as i64,
                raw_nco_at_prev,
                preset.name,
                snapped.round() as i64,
                target_offset,
            );
            snapped.round() as u64
        } else {
            prev_lo as u64
        }
    };

    let gain_mode = match body.gain_mode.as_deref() {
        None => None,
        Some("manual") => Some(crate::hardware::iio::GainMode::Manual),
        Some("slow_attack") => Some(
            crate::hardware::iio::GainMode::SlowAttack),
        Some("fast_attack") => Some(
            crate::hardware::iio::GainMode::FastAttack),
        Some("hybrid") => Some(crate::hardware::iio::GainMode::Hybrid),
        Some(other) => {
            return (StatusCode::BAD_REQUEST, serde_json::json!({
                "ok":    false,
                "error": format!(
                    "unknown gain_mode '{other}'; expected \
                     manual|slow_attack|fast_attack|hybrid"
                ),
            }));
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
    //    crystal-trim correction (`current_lo_shift_hz`) — captured
    //    above for the LO-snap calculation. Re-uses the snapped
    //    `new_rx_lo` so the post-snap NCO stays inside the window.
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
        // 2026-05-03 dual-DDC pivot bug fix: the traffic DDC's FIR
        // coefficients + decimation factors are baked at boot
        // (`main.rs::configure_traffic_ddc(boot_preset)`) and are NOT
        // refreshed on `/api/preset`. Without this call, switching
        // 8M→4M leaves the traffic DDC at decim 8/4/5 against a
        // 4 MSPS input → output 25 kSPS → LsmDecimator2 /2 → 12.5
        // kSPS at the LSM front end (vs the 25 kSPS every LSM
        // submodule is parameterized for). Symptom: control PLL
        // locks fine but traffic PLL never converges, all calls
        // log 0/0/0/0 with no IMBE.
        //
        // Reconfigure with NCO=0 — the next grant's
        // `retune_traffic_chain` writes the real offset; calls
        // mid-flight during a preset change are intentionally
        // dropped (the chain pauses between grants anyway).
        match core.configure_traffic_ddc(0.0, preset) {
            Ok(_) => {
                applied.push(format!(
                    "configure_traffic_ddc preset={}",
                    preset.name));
            }
            Err(e) => errors.push(format!(
                "configure_traffic_ddc: {e}")),
        }
        core.set_traffic_ddc_enable(true);
        // Change 066: the second traffic chain's DDC follows the same
        // preset (present only on core 0.3.0).
        if let Some(l2) = core.lane(crate::hardware::traffic_lane::Lane::Two) {
            match l2.configure_ddc(0.0, preset) {
                Ok(_) => applied.push(format!("configure_traffic2_ddc preset={}", preset.name)),
                Err(e) => errors.push(format!("configure_traffic2_ddc: {e}")),
            }
            l2.set_ddc_enable(true);
        }
    }

    let readback_gain = state.ad9361.get_rx_gain().await.ok();
    let readback_rssi = state.ad9361.get_rx_rssi().await.ok();

    let status = if errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, serde_json::json!({
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
    }))
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
    if let Some(busy) = state.radio_busy() {
        return busy;
    }

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
        // Explicit center-freq command. Operator is telling us where
        // the LO should sit — but if the resulting NCO would be out of
        // the sample-rate window, that's not a valid combination. In
        // auto mode we still snap the LO to bring NCO into range
        // (operator picked the wrong center for this rate); in lock
        // mode we 409 with a clear hint.
        // 2026-05-03: previously this branch unconditionally honored
        // `center_hz` and skipped the window check, leaving the chain
        // in a broken state when (e.g.) a preset apply at 4 MSPS left
        // the LO 2.86 MHz off from the active control freq.
        let c = center as i64;
        let resulting_nco = (radio - c).abs();
        if resulting_nco > usable_half && !lock_req {
            // Snap LO to center the radio freq; ignore the requested
            // center because honoring it would break the chain.
            let rounded = ((radio + TUNE_LO_STEP_HZ / 2) / TUNE_LO_STEP_HZ)
                * TUNE_LO_STEP_HZ;
            tracing::info!(
                target: "p25_tune",
                "tune: requested center_hz={} would put NCO {:+} Hz outside \
                 ±{} Hz window for preset {}; auto-snapping LO to {}",
                c, radio - c, usable_half, preset.name, rounded,
            );
            (rounded, rounded != rx_lo_now)
        } else if resulting_nco > usable_half {
            return (StatusCode::CONFLICT, Json(serde_json::json!({
                "ok":    false,
                "error": "explicit center_hz puts NCO outside locked window",
                "radio_freq_hz":     radio,
                "requested_center":  c,
                "resulting_nco_hz":  radio - c,
                "window_half_hz":    usable_half,
                "preset":            preset.name,
                "hint":              "Unlock center (auto mode) so the LO \
                                      can move, or omit center_hz, or pick \
                                      a wider preset.",
            })));
        } else if c != rx_lo_now {
            (c, true)
        } else {
            (rx_lo_now, false)
        }
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
        let prev = state.current_control_freq.swap(radio as u64, Ordering::Relaxed);
        // Change 070: another control channel may be another system.
        if prev != radio as u64 {
            state.decoder.write().await.new_system();
            state.lsm_decoder.write().await.new_system();
        }
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
    let rt = &state.c4fm_rt;
    Json(serde_json::json!({
        // Change 071b: `mode` is the setting (0 auto, 1 C4FM, 2 LSM);
        // `active` / `label` the decoder publishing now.
        "mode": state.modulation_mode.load(std::sync::atomic::Ordering::Relaxed),
        "active": state.active_modulation.load(std::sync::atomic::Ordering::Relaxed),
        "label": state.active_modulation_label(),
        "tsbk_ok": { "c4fm": c4fm.tsbk_crc_ok, "lsm": lsm.tsbk_crc_ok },
        "tsbk_fail": { "c4fm": c4fm.tsbk_crc_failures, "lsm": lsm.tsbk_crc_failures },
        "c4fm_software": {
            "cpu_pct": rt.cpu_centi_pct.load(std::sync::atomic::Ordering::Relaxed) as f64 / 100.0,
            "chunks": rt.chunks.load(std::sync::atomic::Ordering::Relaxed),
            "lagged": rt.lagged.load(std::sync::atomic::Ordering::Relaxed),
            "resets": rt.resets.load(std::sync::atomic::Ordering::Relaxed),
            "pll_rad_per_symbol": rt.pll_mrad.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0,
            "iq_samples": state.control_iq.samples.load(std::sync::atomic::Ordering::Relaxed),
        },
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
    // Change 071b: the setting; the modulation task applies it within a
    // second (auto keeps choosing by TSBK CRCs).
    state
        .modulation_mode
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
        // What was applied, for the persisted `radio` settings.
        let mut mode_set: Option<String> = None;
        let mut db_set: Option<i32> = None;

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
                            mode_set = Some(new_mode.to_string());
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
                                db_set = Some(db as i32);
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

        // Remember a successful change for the next start (UI settings
        // `radio`, applied after `--hardwaregain` at boot).
        let mut persisted = None;
        if mode_set.is_some() || db_set.is_some() {
            let patch = crate::services::ui_settings::SettingsPatch {
                radio: Some(crate::services::ui_settings::RadioPatch {
                    gain_mode: mode_set,
                    manual_gain_db: db_set,
                }),
                ..Default::default()
            };
            persisted = Some(
                crate::httpd::api::ui::apply_settings_patch(&state, patch, "api_rx_gain")
                    .await
                    .is_ok(),
            );
        }

        Json(serde_json::json!({
            "gain_db":       gain,
            "mode":          mode,
            "rssi_db":       rssi,
            "updated_from":  updated_from,
            "mode_from":     mode_from,
            "persisted":     persisted,
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

    // Tracker diagnostic — shows what the continuous tracker is
    // currently estimating + how many samples it has in its ring.
    // Useful when shift and ppm look right but the tracker is still
    // proposing a correction (or vice-versa).
    let (tracker_samples, tracker_estimate_hz) = {
        let ring = state.ppm_tracker_ring.lock();
        match ring {
            Ok(r) => {
                let n = r.len();
                if n >= 60 {
                    let mut sorted: Vec<f64> = r.iter().copied().collect();
                    sorted.sort_by(|a, b| a.partial_cmp(b)
                        .unwrap_or(std::cmp::Ordering::Equal));
                    let trim = (n as f64 * 0.10).floor() as usize;
                    let kept = &sorted[trim..n - trim];
                    let mean = kept.iter().sum::<f64>() / kept.len() as f64;
                    (n, Some(mean))
                } else { (n, None) }
            }
            Err(_) => (0, None),
        }
    };
    let last_shift_change_ms = state.ppm_last_shift_change_ms
        .load(Ordering::Relaxed);

    let auto_enabled = state.auto_ppm_enabled.load(Ordering::Relaxed);
    let anchor_hz = state.auto_ppm_anchor_hz.load(Ordering::Relaxed);
    let recal_shift = state.last_recal_shift_hz.load(Ordering::Relaxed);
    // Pre-compute whether the current estimate would be applied
    // given the anchor — dashboard can show WHY a non-null estimate
    // isn't moving shift (outside anchor / no recal yet).
    let would_apply = if !auto_enabled {
        serde_json::json!({ "applied": false, "reason": "auto_disabled" })
    } else if anchor_hz == 0 {
        serde_json::json!({ "applied": true, "reason": "unrestricted" })
    } else if recal_shift == 0 {
        serde_json::json!({ "applied": false, "reason": "no_recal_anchor" })
    } else if let Some(est) = tracker_estimate_hz {
        let delta = (est.round() as i64 - recal_shift).abs();
        if delta <= anchor_hz as i64 {
            serde_json::json!({
                "applied": true, "reason": "within_anchor",
                "delta_from_recal_hz": (est.round() as i64 - recal_shift),
            })
        } else {
            serde_json::json!({
                "applied": false, "reason": "outside_anchor",
                "delta_from_recal_hz": (est.round() as i64 - recal_shift),
            })
        }
    } else {
        serde_json::json!({ "applied": false, "reason": "insufficient_samples" })
    };

    Json(serde_json::json!({
        "ok":                     true,
        "lo_shift_hz":            lo_shift_hz,
        "lo_ppm":                 ppm,
        "rx_lo_hz":               rx_lo_hz,
        "boot_lo_ppm":            state.boot_lo_ppm,
        "last_cal_unix_secs":     last_cal,
        "calibrated_this_session": last_cal != 0,
        "tracker_samples":        tracker_samples,
        "tracker_estimate_hz":    tracker_estimate_hz,
        "last_shift_change_ms":   last_shift_change_ms,
        "auto_ppm_enabled":       auto_enabled,
        "auto_ppm_anchor_hz":     anchor_hz,
        "last_recal_shift_hz":    recal_shift,
        "would_apply":            would_apply,
    }))
}

/// `POST /api/ppm/nudge?delta_hz=<n>[&absolute=<shift>]`
///
/// Manual shift adjustment for investigating `pll_dbg` semantics.
/// Either increments current `lo_shift_hz` by `delta_hz` (signed) or
/// sets it absolutely with `absolute=<shift>`. Clamped to ±1 ppm at
/// current `rx_lo`. Reprograms the DDC NCO, bumps the tracker settle
/// timestamp (so the 3 s gate applies), clears the ring.
///
/// Response includes the new shift + current `pll_dbg` + agc_product
/// so a sweep caller can build a `shift vs pll_dbg` table in one go.
/// `pll_residual_hz` reads NCO minus signal: the correct shift is
/// `new_shift_hz - pll_residual_hz` (`autoppm::true_shift_estimate_hz`).
///
/// Linux-only — no AD9361 on host builds.
#[cfg(target_os = "linux")]
pub async fn post_ppm_nudge(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params):
        axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use std::sync::atomic::Ordering;
    let rx_lo = state.current_rx_lo.load(Ordering::Relaxed) as f64;
    let control_freq = state.current_control_freq
        .load(Ordering::Relaxed) as f64;
    let sample_rate = state.current_sample_rate_hz
        .load(Ordering::Relaxed) as f64;
    if rx_lo <= 0.0 || sample_rate <= 0.0 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "ok": false,
            "error": "radio not tuned yet",
        }))).into_response();
    }

    let cur_shift = state.current_lo_shift_hz
        .load(Ordering::Relaxed) as f64;
    let target = if let Some(v) = params.get("absolute") {
        match v.parse::<f64>() {
            Ok(n) => n,
            Err(_) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false, "error": "absolute must be a number (Hz)",
            }))).into_response(),
        }
    } else if let Some(v) = params.get("delta_hz") {
        match v.parse::<f64>() {
            Ok(n) => cur_shift + n,
            Err(_) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false, "error": "delta_hz must be a number (Hz)",
            }))).into_response(),
        }
    } else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
            "ok": false,
            "error": "provide delta_hz=<signed Hz> or absolute=<Hz>",
        }))).into_response();
    };

    // Clamp to ±1 ppm at current rx_lo — same envelope the tracker
    // uses. Protects against fat-fingered absolute=1000000 typos.
    let envelope = rx_lo * 1e-6;
    let new_shift = target.clamp(-envelope, envelope);
    let clamped = (target - new_shift).abs() > 0.5;

    // Reprogram DDC NCO.
    let new_nco = control_freq - rx_lo + new_shift;
    let applied = {
        let core = state.ip_core.lock().await;
        core.set_ddc_frequency(new_nco, sample_rate).is_ok()
    };
    if !applied {
        return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
            "ok": false, "error": "set_ddc_frequency failed",
        }))).into_response();
    }
    state.current_lo_shift_hz.store(
        new_shift.round() as i64, Ordering::Relaxed);
    // Clear tracker ring + arm settle gate so the tracker doesn't
    // immediately try to correct what we just set.
    if let Ok(mut r) = state.ppm_tracker_ring.lock() { r.clear(); }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    state.ppm_last_shift_change_ms.store(now_ms, Ordering::Relaxed);

    // Short settle before reading pll_dbg so caller gets a post-
    // transient snapshot. 300 ms is enough for the Costas to
    // re-lock within ±100 Hz of shift; we still recommend the
    // 3 s settle via /api/ppm polling for a truly steady read.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let (pll_dbg, agc_product) = {
        let core = state.ip_core.lock().await;
        let (pll, _) = core.lsm_debug();
        let (g, m) = core.lsm_agc_debug();
        let agc = (g as f64 / 128.0) * (m as f64 / 32768.0);
        (pll, agc)
    };
    let hz_per_q213 = 4800.0_f64 / (2.0 * std::f64::consts::PI * 8192.0);
    let pll_residual_hz = pll_dbg as f64 * hz_per_q213;
    let new_ppm = -new_shift / (rx_lo * 1e-6);

    (StatusCode::OK, Json(serde_json::json!({
        "ok":                true,
        "old_shift_hz":      cur_shift,
        "new_shift_hz":      new_shift,
        "lo_ppm":            new_ppm,
        "clamped":           clamped,
        "envelope_hz":       envelope,
        "pll_dbg_q213":      pll_dbg,
        "pll_residual_hz":   pll_residual_hz,
        "agc_product":       agc_product,
        "settle_ms_waited":  300,
        "note": "Post-300ms snapshot. Poll /api/ppm for a fully-settled reading.",
    }))).into_response()
}

#[cfg(not(target_os = "linux"))]
pub async fn post_ppm_nudge(
    State(_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let body = serde_json::json!({
        "ok":    false,
        "error": "ppm/nudge requires hardware access (target_os=linux)",
    });
    (StatusCode::NOT_IMPLEMENTED, Json(body)).into_response()
}

/// `POST /api/ppm/auto?enabled=0|1&anchor=<hz>`
///
/// Set the auto-PPM apply gate (checkbox) and/or anchor window.
/// Either parameter is optional. Returns the resulting state.
pub async fn post_ppm_auto(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params):
        axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;
    if let Some(v) = params.get("enabled") {
        let on = v == "1" || v == "true" || v == "on";
        state.auto_ppm_enabled.store(on, Ordering::Relaxed);
    }
    if let Some(v) = params.get("anchor") {
        if let Ok(hz) = v.parse::<u32>() {
            state.auto_ppm_anchor_hz.store(hz, Ordering::Relaxed);
        }
    }
    Json(serde_json::json!({
        "ok":                  true,
        "auto_ppm_enabled":    state.auto_ppm_enabled.load(Ordering::Relaxed),
        "auto_ppm_anchor_hz":  state.auto_ppm_anchor_hz.load(Ordering::Relaxed),
        "last_recal_shift_hz": state.last_recal_shift_hz.load(Ordering::Relaxed),
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
    if let Some(busy) = state.radio_busy() {
        return busy.into_response();
    }
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
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64).unwrap_or(0);
    state.last_ppm_cal_unix_secs.store(now_unix, Ordering::Relaxed);

    let lo_ppm = if rx_lo > 0.0 {
        -(shift_hz as f64) / (rx_lo * 1e-6)
    } else { 0.0 };

    // 2026-05-02: actually persist the override to JFFS2 so the next
    // boot picks it up. Previously the doc-comment claimed persistence
    // but only the auto-PPM tracker code path wrote the file —
    // operator-flagged when manual +470 kept getting overridden by
    // the tracker re-loading 353/394 from disk after reboot.
    let persisted = crate::app::autoppm::PersistedPpm {
        lo_shift_hz:     shift_hz,
        lo_ppm,
        rx_lo_hz:        rx_lo as i64,
        control_freq_hz: state.current_control_freq.load(Ordering::Relaxed),
        unix_secs:       now_unix,
        method:          "manual_override".to_string(),
    };
    let persist_status = match crate::app::autoppm::save_persisted(&persisted) {
        Ok(_) => "persisted".to_string(),
        Err(e) => {
            tracing::warn!(
                "failed to persist manual PPM override: {e} \
                 (live shift still applied)");
            format!("not persisted ({e})")
        }
    };

    // Event-log the override so operators can audit PPM changes via
    // /api/log, not just via tracing output.
    state.event_log.push(
        crate::services::event_log::LogCategory::System,
        format!("manual PPM override: lo_shift_hz={shift_hz:+} \
                 ({lo_ppm:+.4} ppm, baseline reset, {persist_status})"),
        serde_json::json!({
            "kind":           "ppm.override",
            "lo_shift_hz":    shift_hz,
            "lo_ppm":         lo_ppm,
            "nco_offset":     nco_offset,
            "persist_status": persist_status,
        }),
    );

    (StatusCode::OK, Json(serde_json::json!({
        "ok":             true,
        "lo_shift_hz":    shift_hz,
        "lo_ppm":         lo_ppm,
        "nco_offset":     nco_offset,
        "persist_status": persist_status,
        "note":           format!(
            "shift applied live and persistence: {persist_status}"),
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
/// Returns the current threshold for every chain (`control` /
/// `traffic`, plus `traffic2` on core 0.3.0, else null) and Q1.15
/// float equivalents. `?chain=` param is
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
    let trf2 = core.lane(crate::hardware::traffic_lane::Lane::Two).map(|l| l.agc_threshold());
    Json(serde_json::json!({
        "ok":              true,
        "control":         ctrl,
        "control_f":       (ctrl as f64) / 32768.0,
        "traffic":         trf,
        "traffic_f":       (trf as f64) / 32768.0,
        "traffic2":        trf2,
        "valid_range":     [0, 65535],
        "default":         256,
        "note":            "Q1.15 raw; 256 = -42 dBFS; 0 disables gate",
    }))
}

/// `PUT /api/agc_threshold?chain=control|traffic|traffic1|traffic2|both&value=<u16>`
///
/// Writes the threshold for the selected chain. `traffic` sets every
/// traffic chain (change 066: both on core 0.3.0), `traffic1` /
/// `traffic2` one of them, and `both` the control chain and every
/// traffic chain.
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
    use crate::hardware::traffic_lane::Lane;
    let core = state.ip_core.lock().await;
    let (ctrl, lanes): (bool, &[Lane]) = match chain {
        "control" => (true, &[]),
        "traffic" => (false, &Lane::ALL),
        "traffic1" => (false, &[Lane::One]),
        "traffic2" => (false, &[Lane::Two]),
        "both" => (true, &Lane::ALL),
        other => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({
                "ok": false,
                "error": format!(
                    "unknown chain '{other}'; expected control|traffic|traffic1|traffic2|both"),
            }))).into_response();
        }
    };
    if lanes == [Lane::Two] && !core.has_traffic2() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({
            "ok": false,
            "error": format!("core {} has no second traffic chain", core.core_version()),
        }))).into_response();
    }
    if ctrl {
        core.set_lsm_agc_threshold(v);
    }
    let mut applied_trf = [false; 2];
    for l in lanes.iter().filter_map(|&l| core.lane(l)) {
        l.set_agc_threshold(v);
        applied_trf[l.lane().index()] = true;
    }
    let ctrl_now = core.lsm_agc_threshold();
    let trf_now  = core.traffic_lsm_agc_threshold();
    let trf2_now = core.lane(Lane::Two).map(|l| l.agc_threshold());
    (StatusCode::OK, Json(serde_json::json!({
        "ok":               true,
        "applied_control":  ctrl,
        "applied_traffic":  applied_trf[0],
        "applied_traffic2": applied_trf[1],
        "control":          ctrl_now,
        "traffic":          trf_now,
        "traffic2":         trf2_now,
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

