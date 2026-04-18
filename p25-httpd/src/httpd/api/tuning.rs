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
    Json,
};

#[allow(unused_imports)]
use p25_json::*;

#[allow(unused_imports)]
use crate::httpd::AppState;
#[allow(unused_imports)]
use crate::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, SYNC_THRESHOLD,
};

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
pub async fn get_reinit(
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
pub async fn get_reinit(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "front-end re-init is only available on the target (linux/arm)",
    }))
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
        use crate::iio::GainMode;

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
                                crate::event_log::LogCategory::System,
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


/// GET /api/bch_t -- read current BCH-t override for both decoder sides.
pub async fn get_bch_t(
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


