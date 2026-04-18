//! Visual diagnostics: spectrum, constellation.
//!
//! Consumer orientation: "show me what the signal looks like right
//! now." Two endpoints, both returning arrays that an SDR-savvy
//! consumer renders as a 2-D plot:
//!
//!   - `/api/spectrum` — FFT magnitudes from the IQ capture ring.
//!     Hand-rolled Cooley-Tukey under the hood (no external FFT
//!     dep — see `src/spectrum.rs`). Accepts `?chain=control|traffic`
//!     and `?fft=<512|1024|2048|4096>`.
//!   - `/api/constellation` — raw IQ scatter from the LSM slicer
//!     input. Interpretation notes live in project memory
//!     `reference_p25_constellation_interpretation`.
//!
//! The dashboard's Debug tab renders both. A diagnostic session on
//! 2026-04-17 (see `doc/diagnostics/2026-04-17/PERFORMANCE_ANALYSIS.md`)
//! caught a Gardner-TED timing-loop problem purely by watching the
//! constellation X-pattern — this endpoint set is load-bearing for
//! field debugging.

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

/// `GET /api/spectrum?chain=control|traffic`
///
/// Runs an N-point FFT (default 4096) over the most recent IQ ring
/// samples from the selected chain and returns the magnitude
/// spectrum in dBFS, fftshifted so bin 0 is the most-negative
/// frequency (-sample_rate_hz/2 relative to the chain's DDC center).
///
/// Query params:
///   - `chain` — `control` (default) or `traffic`
///   - `fft`   — one of {1024, 2048, 4096, 8192, 16384}; default 4096
///   - `averages` — 1..floor(65536/fft); default 1. Power-averages N
///     non-overlapping FFT segments (noise floor drops by
///     ~10·log10(averages) dB, carriers stay put). Set 1 for live
///     sweep, 4–16 for noise-floor / channel-shape analysis.
///
/// The traffic chain requires the bake #2 bitstream flashed; on
/// older binaries the endpoint returns an error explaining the
/// missing UIO device. The control chain works on any bitstream
/// that has the Phase 6C `iq_dma` ring.
#[cfg(target_os = "linux")]
pub async fn get_spectrum(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    let chain = params
        .get("chain")
        .map(String::as_str)
        .unwrap_or("control");

    let fft_size = crate::spectrum::clamp_fft_size(
        params.get("fft").and_then(|s| s.parse::<usize>().ok()),
    );
    let averages = crate::spectrum::clamp_averages(
        params.get("averages").and_then(|s| s.parse::<usize>().ok()),
        fft_size,
    );
    let min_samples = fft_size * averages;
    let min_bytes = min_samples * 4;

    // Pull buffers from the requested ring. read_*_buffers() is a
    // rolling-window reader — call it once, accumulate what it
    // returned, then retry with the lock released if we still don't
    // have enough samples. For averages=1, fft_size=4096 this is the
    // original ~65 ms snapshot; for averages=16 it can need up to
    // ~1 s of ring data.
    let bytes: Vec<u8> = {
        let mut acc: Vec<u8> = Vec::with_capacity(min_bytes);
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_millis(1500);
        loop {
            {
                let mut core = state.ip_core.lock().await;
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
                for b in bufs {
                    acc.extend_from_slice(b);
                }
            }
            if acc.len() >= min_bytes {
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
        acc
    };

    let Some(snap) = crate::spectrum::spectrum_from_bytes(&bytes, fft_size, averages) else {
        return Json(serde_json::json!({
            "ok": false,
            "error": format!(
                "not enough IQ samples for fft={fft_size} on chain={chain} \
                 ({} samples available, need at least {fft_size})",
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
        "fft_size":        snap.fft_size,
        "averages_used":   snap.averages_used,
        "mag_db":          snap.mag_db,
    }))
}


#[cfg(not(target_os = "linux"))]
pub async fn get_spectrum(
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
pub async fn get_constellation(
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
pub async fn get_constellation(
    State(_state): State<Arc<AppState>>,
    Query(_params): Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "ok": false,
        "error": "constellation only available on the target (linux/arm)",
    }))
}


